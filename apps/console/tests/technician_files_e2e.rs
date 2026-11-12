//! The technician's own file actions, end to end (ST-M010-001).
//!
//! One real loopback session joins a console to an in-process agent that
//! serves its real file store. The technician's actions — the drive list, the
//! removal of a folder with its content, a file sent into a folder — go
//! through [`TechnicianFiles`], and each test looks at the three places an
//! action must show up in: the report the caller gets, the console's audit
//! records, and what the customer's side shows (REQ-F-017, REQ-S-017,
//! REQ-N-008).
//!
//! The wire accepts an absolute path only when it is rooted on a drive, so a
//! file can be committed in a folder only where such a path names a real
//! folder: that test runs on Windows. The tests that run everywhere send
//! nothing into a folder; they cover what is refused, cancelled or fails
//! before a byte is written.
//!
//! `unwrap`/`expect`/`panic` are allowed here: test-only code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cretlink_console::{
    AuditActorView, AuditEntry, AuditServiceConfig, ChannelAudit, ConsoleSessionService, IpcAudit,
    ProgressSink, SendToFolder, TechnicianActivity, TechnicianFiles, open_session_log,
    read_session, session_entries, spawn_audit_service,
};
use cretlink_exec::FileStore;
use cretlink_mcp::TargetError;
use cretlink_protocol::limits::MAX_TREE_STEP_ENTRIES;
use cretlink_protocol::{
    CAP_FS_V1, CAP_FS_V2, ErrorCode, ErrorStage, JobOutcome, PROTOCOL_V1, SessionId, TargetIdentity,
};
use cretlink_session::{
    ActionOutcome, ActivityEntry, ActivityKind, ActivitySource, ActivityState, CancelToken,
    ConsoleHints, HostHandle, JoinHandle, Origin, RemotePath, SessionAudit, SessionEvent,
    SessionHost, SessionJoiner, TreeRemovalProgress,
};
use cretlink_shared::{
    Actor, AuditRecord, AuditValue, ChainReport, Outcome, RetentionPolicy, verify_sealed,
};
use tokio::sync::oneshot;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(10);
const SEAL: Duration = Duration::from_secs(30);

/// A folder on the customer computer that the tests which run everywhere name
/// and never write to.
const DOCUMENTS: &str = "C:\\Users\\ann\\Documents";

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn agent_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "CUSTOMER-PC".to_string(),
        os_version: "Simulated Windows".to_string(),
        arch: "x86_64".to_string(),
        elevated: false,
        simulated: true,
        agent_version: "technician-files-e2e-agent".to_string(),
        account: None,
    }
}

/// What the support agent advertises: the file capability and the explorer
/// file capability.
fn explorer_caps() -> Vec<String> {
    vec![CAP_FS_V1.to_string(), CAP_FS_V2.to_string()]
}

fn temp_dir_for(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cretlink-technician-files-{test}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A live, consented session: the console's service, and the agent's handle
/// whose events are what the customer's side shows.
struct Live {
    service: ConsoleSessionService,
    joiner: JoinHandle,
    session: SessionId,
}

async fn next_host_event(handle: &mut HostHandle) -> SessionEvent {
    timeout(STEP, handle.next_event())
        .await
        .expect("timed out waiting for a session event")
        .expect("session event stream ended unexpectedly")
}

/// Pairs a console with a consenting in-process agent that advertises
/// `agent_caps`, and starts the console's session service on it.
async fn live_session(agent_caps: Vec<String>, audit: Option<Arc<dyn SessionAudit>>) -> Live {
    let hints = ConsoleHints {
        label: "This computer (technician)".to_string(),
        display_name: None,
    };
    let mut host = match audit {
        Some(audit) => {
            SessionHost::host_with_audit(loopback(), hints, PROTOCOL_V1, Vec::new(), audit).await
        }
        None => SessionHost::host(loopback(), hints, PROTOCOL_V1, Vec::new()).await,
    }
    .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join(addr, agent_identity(), PROTOCOL_V1, agent_caps)
        .await
        .expect("dial agent");

    assert_eq!(next_host_event(&mut host).await, SessionEvent::Connected);
    assert_eq!(
        next_host_event(&mut host).await,
        SessionEvent::AwaitingApprovalCompare
    );
    host.confirm_code().expect("confirm the approval code");
    assert_eq!(
        next_host_event(&mut host).await,
        SessionEvent::AwaitingConsent
    );
    joiner.grant_consent().expect("grant consent");
    let identity = match next_host_event(&mut host).await {
        SessionEvent::ConsentGranted { identity } => identity,
        other => panic!("expected ConsentGranted, got {other:?}"),
    };
    assert_eq!(next_host_event(&mut host).await, SessionEvent::Active);
    let session = host.session().expect("session id once active");
    let (service, _driver) = ConsoleSessionService::start(host, session, identity);
    Live {
        service,
        joiner,
        session,
    }
}

/// What the customer's side showed for one handled request.
#[derive(Debug, PartialEq)]
struct Shown {
    origin: Origin,
    kind: ActivityKind,
    operation: String,
    path: Option<String>,
    bytes: Option<u64>,
    outcome: ActionOutcome,
}

/// The next request the agent reports as handled, skipping its other events.
async fn next_shown(joiner: &mut JoinHandle) -> Shown {
    loop {
        let event = timeout(STEP, joiner.next_event())
            .await
            .expect("timed out waiting for what the customer's side shows")
            .expect("the agent's event stream ended unexpectedly");
        if let SessionEvent::RequestHandled {
            origin,
            kind,
            operation,
            path,
            bytes,
            outcome,
            ..
        } = event
        {
            return Shown {
                origin,
                kind,
                operation,
                path,
                bytes,
                outcome,
            };
        }
    }
}

/// An audit sink that keeps every entry in memory, in order.
#[derive(Default)]
struct Trail(Mutex<Vec<AuditEntry>>);

impl IpcAudit for Trail {
    fn record(&self, entry: AuditEntry) {
        self.0.lock().unwrap().push(entry);
    }
}

impl Trail {
    fn all(&self) -> Vec<AuditEntry> {
        self.0.lock().unwrap().clone()
    }

    fn of(&self, action: &str) -> Vec<AuditEntry> {
        self.all()
            .into_iter()
            .filter(|entry| entry.action == action)
            .collect()
    }

    /// The start record and the completion record of the one action recorded
    /// under these names: both the technician's, the start first, sharing one
    /// `action_id`, the completion with its duration.
    fn pair(&self, action_start: &str, action: &str) -> (AuditEntry, AuditEntry) {
        let all = self.all();
        let position = |name: &str| {
            let found: Vec<usize> = all
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.action == name)
                .map(|(at, _)| at)
                .collect();
            assert_eq!(found.len(), 1, "exactly one {name} record: {all:?}");
            found[0]
        };
        let (start_at, done_at) = (position(action_start), position(action));
        assert!(start_at < done_at, "the start is recorded first");
        let (start, done) = (all[start_at].clone(), all[done_at].clone());
        assert_eq!(start.actor, Actor::Console);
        assert_eq!(done.actor, Actor::Console);
        assert_eq!(start.outcome, Outcome::Ok, "a start that began");
        assert!(int(&start, "action_id").is_some(), "{start:?}");
        assert_eq!(int(&start, "action_id"), int(&done, "action_id"));
        assert!(done.duration_ms.is_some());
        (start, done)
    }
}

fn int(entry: &AuditEntry, key: &str) -> Option<i64> {
    match entry.args.0.get(key) {
        Some(AuditValue::Int(value)) => Some(*value),
        _ => None,
    }
}

fn text(entry: &AuditEntry, key: &str) -> Option<String> {
    entry.args.0.get(key).map(ToString::to_string)
}

/// Plants the folder `name` in the staging area of the session's in-process
/// agent and returns its real path. The agent roots its store under
/// `FileStore::for_session(session)`, so a second instance for the same id
/// yields the identical staging root.
fn staged_folder(session: SessionId, name: &str) -> PathBuf {
    let folder = FileStore::for_session(session).staging_root().join(name);
    std::fs::create_dir_all(&folder).expect("plant the folder");
    folder
}

/// The wire name of a folder planted with [`staged_folder`].
fn staged(name: &str) -> RemotePath {
    RemotePath::Staging {
        name: name.to_string(),
    }
}

/// The words both sides use for the removal of the staged folder `name`.
fn removal_of(name: &str) -> String {
    format!("delete the folder {name} (CretLink staging folder) and everything in it")
}

/// Writes `count` small files into `folder`.
fn plant_files(folder: &Path, count: u64) {
    for n in 0..count {
        std::fs::write(folder.join(format!("file-{n:05}.txt")), b"x").expect("plant a file");
    }
}

/// How many entries `folder` holds directly.
fn entries_in(folder: &Path) -> u64 {
    std::fs::read_dir(folder).map_or(0, |entries| entries.count() as u64)
}

/// A small file on the technician computer, to send.
fn local_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write the local file");
    path
}

fn send(local_path: PathBuf, folder: &str, name: &str, overwrite: bool) -> SendToFolder {
    SendToFolder {
        local_path,
        folder: folder.to_string(),
        name: name.to_string(),
        overwrite,
    }
}

/// The drives are listed for the technician, recorded as the technician's
/// action, shown to the customer as the technician's, and they are one item
/// of the technician's own activity feed.
#[tokio::test]
async fn the_technician_lists_the_drives() {
    let mut live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);

    let drives = files.list_drives().await.expect("the drives are listed");
    assert!(!drives.is_empty(), "a computer has at least one drive");
    // Off Windows the one drive is the root file system.
    #[cfg(unix)]
    assert_eq!(
        drives.iter().map(|d| d.root.as_str()).collect::<Vec<_>>(),
        ["/"]
    );

    let (start, done) = trail.pair("console_fs_drives_start", "console_fs_drives");
    assert_eq!(start.session, Some(live.session));
    assert_eq!(done.session, Some(live.session));
    assert_eq!(done.outcome, Outcome::Ok);
    assert_eq!(int(&done, "drives"), i64::try_from(drives.len()).ok());

    assert_eq!(
        next_shown(&mut live.joiner).await,
        Shown {
            origin: Origin::Console,
            kind: ActivityKind::FileOperation,
            operation: "list the drives".to_string(),
            path: None,
            bytes: None,
            outcome: ActionOutcome::Succeeded,
        }
    );

    let mut fold = TechnicianActivity::new();
    for entry in trail.all() {
        fold.apply(&entry, 0);
    }
    match fold.entries().as_slice() {
        [ActivityEntry::Item { item }] => {
            assert_eq!(item.source, ActivitySource::Technician);
            assert_eq!(item.kind, ActivityKind::FileOperation);
            assert_eq!(item.operation, "list the drives");
            assert_eq!(item.state, ActivityState::Done { exit_code: None });
        }
        other => panic!("expected one item of the technician, got {other:?}"),
    }
    live.service.stop();
}

/// One call removes a folder with everything in it. The report, the audit
/// records and the customer's side agree: one action of the technician, with
/// its path, done because the folder is gone.
#[tokio::test]
async fn the_technician_removes_a_folder_with_its_content() {
    let mut live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let folder = staged_folder(live.session, "old-setup");
    plant_files(&folder, 5);
    std::fs::create_dir_all(folder.join("sub").join("deeper")).expect("plant");
    plant_files(&folder.join("sub").join("deeper"), 2);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink: ProgressSink<TreeRemovalProgress> = {
        let seen = Arc::clone(&seen);
        Box::new(move |step| seen.lock().unwrap().push(step))
    };
    let report = files
        .remove_tree(staged("old-setup"), CancelToken::new(), Some(sink))
        .await
        .expect("the removal runs");

    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(report.removed_files, 7);
    assert_eq!(report.removed_dirs, 3, "deeper, sub and the folder itself");
    assert!(report.failed.is_empty() && report.failed_more == 0);
    assert!(!folder.exists(), "the folder is gone");
    assert_eq!(
        seen.lock().unwrap().last(),
        Some(&TreeRemovalProgress {
            removed_files: 7,
            removed_dirs: 3,
        }),
        "the caller followed the removal to its end"
    );

    let (start, done) = trail.pair("console_remove_tree_start", "console_remove_tree");
    assert_eq!(text(&start, "path").as_deref(), Some("staging:old-setup"));
    assert_eq!(text(&done, "path").as_deref(), Some("staging:old-setup"));
    assert_eq!(done.outcome, Outcome::Ok);
    assert_eq!(int(&done, "removed_files"), Some(7));
    assert_eq!(int(&done, "removed_dirs"), Some(3));
    assert_eq!(int(&done, "not_removed"), None);
    assert_eq!(text(&done, "reason"), None);

    assert_eq!(
        next_shown(&mut live.joiner).await,
        Shown {
            origin: Origin::Console,
            kind: ActivityKind::FileOperation,
            operation: removal_of("old-setup"),
            path: Some("old-setup (CretLink staging folder)".to_string()),
            bytes: None,
            outcome: ActionOutcome::Succeeded,
        }
    );
    live.service.stop();
}

/// The technician cancels a removal that takes several steps: it stops
/// between two of them, says what it had removed, and is cancelled in the
/// report, in the audit trail and on the customer's side — never done.
#[tokio::test]
async fn a_cancelled_removal_stops_and_is_recorded_as_cancelled() {
    let mut live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let folder = staged_folder(live.session, "large");
    let planted = MAX_TREE_STEP_ENTRIES * 2 + 50;
    plant_files(&folder, planted);

    // Cancelled as soon as the first step has been answered.
    let cancel = CancelToken::new();
    let sink: ProgressSink<TreeRemovalProgress> = {
        let cancel = cancel.clone();
        Box::new(move |_| cancel.cancel())
    };
    let report = files
        .remove_tree(staged("large"), cancel, Some(sink))
        .await
        .expect("a cancelled removal is a report");

    assert_eq!(report.outcome, JobOutcome::Cancelled);
    assert!(
        report.removed_files > 0 && report.removed_files < planted,
        "{report:?}"
    );
    assert_eq!(report.removed_dirs, 0);
    assert!(report.failed.is_empty() && report.error.is_none());
    assert_eq!(
        entries_in(&folder),
        planted - report.removed_files,
        "nothing more was removed after the cancellation"
    );

    let (_, done) = trail.pair("console_remove_tree_start", "console_remove_tree");
    assert_eq!(done.outcome, Outcome::Cancelled);
    assert_eq!(
        int(&done, "removed_files"),
        i64::try_from(report.removed_files).ok()
    );

    // One action for the customer: in progress, then cancelled.
    let begun = next_shown(&mut live.joiner).await;
    assert_eq!(begun.origin, Origin::Console);
    assert_eq!(begun.operation, removal_of("large"));
    assert_eq!(begun.outcome, ActionOutcome::InProgress);
    let ended = next_shown(&mut live.joiner).await;
    assert_eq!(ended.operation, removal_of("large"));
    assert_eq!(ended.outcome, ActionOutcome::Cancelled);

    live.service.stop();
    let _ = std::fs::remove_dir_all(&folder);
}

/// Something that keeps an entry of `folder` from being removed, for as long
/// as it lives, and the reason the agent gives for it.
struct Held {
    #[cfg(windows)]
    _open: std::fs::File,
    #[cfg(unix)]
    sealed: PathBuf,
}

impl Held {
    /// Plants `folder/held/kept.bin` and makes it unremovable. `None` when
    /// this account cannot be held back (it is the superuser).
    #[cfg(unix)]
    fn plant(folder: &Path) -> Option<(Self, PathBuf, ErrorCode)> {
        use std::os::unix::fs::PermissionsExt;
        let sealed = folder.join("held");
        std::fs::create_dir_all(&sealed).expect("plant");
        let kept = sealed.join("kept.bin");
        std::fs::write(&kept, b"kept").expect("plant");
        // Without write access to the folder nothing in it can be removed.
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o555)).expect("seal");
        if std::fs::write(sealed.join(".probe"), b"").is_ok() {
            let _ = std::fs::remove_file(sealed.join(".probe"));
            eprintln!("skipped: this account is not held back by permissions");
            return None;
        }
        Some((Self { sealed }, kept, ErrorCode::AccessDenied))
    }

    /// Plants `folder/held/kept.bin` and holds it open the way a program
    /// does that does not let others delete its file.
    #[cfg(windows)]
    fn plant(folder: &Path) -> Option<(Self, PathBuf, ErrorCode)> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        std::fs::create_dir_all(folder.join("held")).expect("plant");
        let kept = folder.join("held").join("kept.bin");
        std::fs::write(&kept, b"kept").expect("plant");
        let open = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&kept)
            .expect("hold the file open");
        Some((Self { _open: open }, kept, ErrorCode::FileInUse))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.sealed, std::fs::Permissions::from_mode(0o755));
        }
    }
}

/// A removal that cannot remove everything is a failure everywhere: the
/// report names what was left, the audit record counts it and gives the first
/// reason, and the customer's side shows a failure. The folder is still
/// there, so nothing says it was removed (REQ-N-008).
#[tokio::test]
async fn a_removal_that_leaves_something_behind_is_recorded_as_failed() {
    let mut live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let folder = staged_folder(live.session, "in-use");
    plant_files(&folder, 3);
    let Some((held, kept, code)) = Held::plant(&folder) else {
        live.service.stop();
        let _ = std::fs::remove_dir_all(&folder);
        return;
    };

    let report = files
        .remove_tree(staged("in-use"), CancelToken::new(), None)
        .await
        .expect("a removal that left something is a report");

    assert_eq!(report.outcome, JobOutcome::Failed);
    assert_eq!(report.removed_files, 3, "what could be removed is gone");
    assert!(kept.exists() && folder.exists());
    let first = report.failed.first().expect("what was left is named");
    assert_eq!(first.code, code, "{:?}", report.failed);
    assert!(first.path.ends_with("kept.bin"), "{first:?}");

    let (_, done) = trail.pair("console_remove_tree_start", "console_remove_tree");
    assert_eq!(done.outcome, Outcome::Failed);
    assert_eq!(int(&done, "removed_files"), Some(3));
    let not_removed = report.failed.len() as u64 + report.failed_more;
    assert_eq!(int(&done, "not_removed"), i64::try_from(not_removed).ok());
    let reason = text(&done, "reason").expect("a failed removal says why");
    assert!(reason.contains("could not be removed"), "{reason}");
    assert!(reason.contains("kept.bin"), "{reason}");
    assert!(reason.contains(&format!("{code:?}")), "{reason}");

    let shown = next_shown(&mut live.joiner).await;
    assert_eq!(shown.operation, removal_of("in-use"));
    assert_eq!(shown.outcome, ActionOutcome::Failed { code });

    live.service.stop();
    drop(held);
    let _ = std::fs::remove_dir_all(&folder);
}

/// A folder that is not there is a failed removal with the target's reason,
/// recorded as failed.
#[tokio::test]
async fn a_folder_that_does_not_exist_is_a_failed_removal() {
    let mut live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    // The staging area exists; the folder in it does not.
    let _ = staged_folder(live.session, "other");

    let report = files
        .remove_tree(staged("never-there"), CancelToken::new(), None)
        .await
        .expect("a refused removal is a report");
    assert_eq!(report.outcome, JobOutcome::Failed);
    assert_eq!(
        report.error.as_ref().map(|error| error.code),
        Some(ErrorCode::PathNotFound)
    );
    assert_eq!((report.removed_files, report.removed_dirs), (0, 0));

    let (_, done) = trail.pair("console_remove_tree_start", "console_remove_tree");
    assert_eq!(done.outcome, Outcome::Failed);
    let reason = text(&done, "reason").expect("a failed removal says why");
    assert!(reason.starts_with("PathNotFound"), "{reason}");

    let shown = next_shown(&mut live.joiner).await;
    assert_eq!(
        shown.outcome,
        ActionOutcome::Failed {
            code: ErrorCode::PathNotFound
        }
    );
    live.service.stop();
}

/// An agent that does not advertise the explorer file capability refuses the
/// drive list and the folder removal; both refusals are recorded as failed and
/// nothing is removed (REQ-N-012).
#[tokio::test]
async fn without_the_capability_the_actions_are_refused_and_recorded() {
    let live = live_session(vec![CAP_FS_V1.to_string()], None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let folder = staged_folder(live.session, "kept");
    plant_files(&folder, 3);

    match files.list_drives().await {
        Err(TargetError::Remote { code, .. }) => assert_eq!(code, ErrorCode::Unsupported),
        other => panic!("expected an Unsupported remote error, got {other:?}"),
    }
    let (_, done) = trail.pair("console_fs_drives_start", "console_fs_drives");
    assert_eq!(done.outcome, Outcome::Failed);
    assert!(
        text(&done, "reason").is_some_and(|reason| reason.contains("Unsupported")),
        "{done:?}"
    );
    assert_eq!(int(&done, "drives"), None);

    let report = files
        .remove_tree(staged("kept"), CancelToken::new(), None)
        .await
        .expect("a refused removal is a report");
    assert_eq!(report.outcome, JobOutcome::Failed);
    assert_eq!(
        report.error.as_ref().map(|error| error.code),
        Some(ErrorCode::Unsupported)
    );
    assert_eq!(entries_in(&folder), 3, "nothing was removed");
    let (_, done) = trail.pair("console_remove_tree_start", "console_remove_tree");
    assert_eq!(done.outcome, Outcome::Failed);
    assert!(
        text(&done, "reason").is_some_and(|reason| reason.starts_with("Unsupported")),
        "{done:?}"
    );

    live.service.stop();
    let _ = std::fs::remove_dir_all(&folder);
}

/// Stopping the session ends a removal between two of its steps: it is never
/// reported or recorded as done, nothing more is removed, and every later
/// action fails closed and is still recorded (REQ-S-004, REQ-S-009).
#[tokio::test]
async fn a_stop_ends_a_removal_and_later_actions_fail_closed() {
    let live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let folder = staged_folder(live.session, "stopped");
    let planted = MAX_TREE_STEP_ENTRIES * 2 + 50;
    plant_files(&folder, planted);

    // The technician stops the session as soon as the first step is answered.
    let sink: ProgressSink<TreeRemovalProgress> = {
        let service = live.service.clone();
        Box::new(move |_| service.stop())
    };
    let result = files
        .remove_tree(staged("stopped"), CancelToken::new(), Some(sink))
        .await;
    match &result {
        Ok(report) => assert_eq!(report.outcome, JobOutcome::Cancelled, "{report:?}"),
        Err(error) => assert!(
            matches!(
                error,
                TargetError::NoConsentedSession | TargetError::Disconnected
            ),
            "{error:?}"
        ),
    }
    let left = entries_in(&folder);
    assert!(
        left > 0 && left < planted,
        "one step ran and no other: {left} of {planted} left"
    );
    let (_, done) = trail.pair("console_remove_tree_start", "console_remove_tree");
    assert_ne!(done.outcome, Outcome::Ok);
    assert!(
        int(&done, "removed_files").is_some_and(|removed| removed > 0),
        "what was removed until the stop is recorded: {done:?}"
    );

    assert!(!live.service.is_active());
    assert_eq!(
        files.list_drives().await,
        Err(TargetError::NoConsentedSession)
    );
    let (_, refused) = trail.pair("console_fs_drives_start", "console_fs_drives");
    assert_eq!(refused.outcome, Outcome::Failed);
    assert!(matches!(
        files
            .remove_tree(staged("stopped"), CancelToken::new(), None)
            .await,
        Err(TargetError::NoConsentedSession)
    ));
    let dir = temp_dir_for("after-stop");
    let source = local_file(&dir, "notes.txt", b"notes");
    assert!(matches!(
        files
            .send_to_folder(
                send(source, DOCUMENTS, "notes.txt", false),
                CancelToken::new(),
                None
            )
            .await,
        Err(TargetError::NoConsentedSession)
    ));
    let sends = trail.of("console_send");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].outcome, Outcome::Failed);
    assert_eq!(entries_in(&folder), left, "nothing was removed afterwards");

    let _ = std::fs::remove_dir_all(&folder);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name that is a path is refused before anything is sent: the call fails
/// with `invalid_path`, one failed record says so, and the customer's
/// computer never hears of it.
#[tokio::test]
async fn a_send_whose_name_is_a_path_is_refused_before_it_begins() {
    let mut live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let dir = temp_dir_for("refused-name");
    let source = local_file(&dir, "notes.txt", b"notes");

    match files
        .send_to_folder(
            send(source, DOCUMENTS, "..\\notes.txt", false),
            CancelToken::new(),
            None,
        )
        .await
    {
        Err(TargetError::Remote {
            code: ErrorCode::InvalidPath,
            stage: ErrorStage::Validation,
            ..
        }) => {}
        other => panic!("expected a locally refused path, got {other:?}"),
    }

    let all = trail.all();
    assert_eq!(all.len(), 1, "{all:?}");
    let refused = &all[0];
    assert_eq!(refused.action, "console_send_start");
    assert_eq!(refused.actor, Actor::Console);
    assert_eq!(refused.outcome, Outcome::Failed);
    assert_eq!(
        text(refused, "path").as_deref(),
        Some("C:\\Users\\ann\\Documents\\..\\notes.txt"),
        "the path is recorded as it was asked for"
    );
    assert!(text(refused, "reason").is_some());

    // The next thing the customer's side shows is the drive list asked for
    // now: nothing of the refused send reached it.
    files.list_drives().await.expect("the session still works");
    assert_eq!(
        next_shown(&mut live.joiner).await.operation,
        "list the drives"
    );
    live.service.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file that cannot be read on the technician computer is a failed
/// transfer, recorded as failed, that never reaches the customer's computer
/// and leaves the session usable.
#[tokio::test]
async fn a_send_whose_local_file_is_missing_fails_without_reaching_the_target() {
    let mut live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let dir = temp_dir_for("missing-source");

    let report = files
        .send_to_folder(
            send(dir.join("not-there.pdf"), DOCUMENTS, "report.pdf", false),
            CancelToken::new(),
            None,
        )
        .await
        .expect("a failed transfer is a report");
    assert_eq!(report.outcome, JobOutcome::Failed);
    assert_eq!(
        report.error.as_ref().map(|error| error.stage.as_str()),
        Some("local")
    );
    assert_eq!(report.remote_path, None);

    let (start, done) = trail.pair("console_send_start", "console_send");
    assert_eq!(
        text(&start, "path").as_deref(),
        Some("C:\\Users\\ann\\Documents\\report.pdf")
    );
    assert!(!start.args.0.contains_key("overwrite"));
    assert_eq!(done.outcome, Outcome::Failed);
    assert!(text(&done, "reason").is_some());
    assert!(done.digests.is_empty(), "nothing was verified");

    files.list_drives().await.expect("the session still works");
    assert_eq!(
        next_shown(&mut live.joiner).await.operation,
        "list the drives"
    );
    live.service.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A send the technician cancelled before it started writes nothing and is
/// recorded as cancelled, with the request to replace an existing file kept
/// in the record.
#[tokio::test]
async fn a_send_cancelled_before_it_starts_is_recorded_as_cancelled() {
    let live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let dir = temp_dir_for("cancelled-send");
    let source = local_file(&dir, "notes.txt", b"some notes");

    let cancel = CancelToken::new();
    cancel.cancel();
    let report = files
        .send_to_folder(send(source, DOCUMENTS, "notes.txt", true), cancel, None)
        .await
        .expect("a cancelled transfer is a report");
    assert_eq!(report.outcome, JobOutcome::Cancelled);
    assert_eq!(report.sent, 0);
    assert_eq!(report.remote_path, None);

    let (start, done) = trail.pair("console_send_start", "console_send");
    assert_eq!(
        start.args.0.get("overwrite"),
        Some(&AuditValue::Bool(true)),
        "a replacement that was asked for is on record"
    );
    assert_eq!(done.outcome, Outcome::Cancelled);
    assert_eq!(int(&done, "len"), Some(10));
    assert!(done.digests.is_empty(), "nothing was verified");

    live.service.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file is sent into a folder: it appears there under its name with its
/// content, verified, and nowhere else; it is not staged; an existing file is
/// replaced only on request. The audit records carry the committed path, the
/// length and the digest, and the customer's side shows a received file from
/// the technician. Windows only: the wire accepts drive-qualified paths only.
#[cfg(windows)]
#[tokio::test]
async fn a_file_is_sent_into_a_folder_verified_and_replaced_only_on_request() {
    let mut live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let dir = temp_dir_for("send-to-folder");
    let documents = dir.join("Documents");
    std::fs::create_dir_all(&documents).expect("create the folder");
    let folder = documents.display().to_string();
    let final_path = documents.join("abc.txt");
    let names = || {
        let mut names: Vec<String> = std::fs::read_dir(&documents)
            .expect("list")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    };

    // The SHA-256 of "abc", a published test vector.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink: ProgressSink<cretlink_session::TransferProgress> = {
        let seen = Arc::clone(&seen);
        Box::new(move |step| seen.lock().unwrap().push(step))
    };
    let report = files
        .send_to_folder(
            send(
                local_file(&dir, "source.txt", b"abc"),
                &folder,
                "abc.txt",
                false,
            ),
            CancelToken::new(),
            Some(sink),
        )
        .await
        .expect("the transfer runs");
    assert_eq!(report.outcome, JobOutcome::Completed, "{:?}", report.error);
    assert_eq!(report.len, 3);
    assert_eq!(std::fs::read(&final_path).expect("read"), b"abc");
    assert_eq!(names(), ["abc.txt"], "no part is left beside the file");
    assert!(!seen.lock().unwrap().is_empty(), "progress was reported");

    let (start, done) = trail.pair("console_send_start", "console_send");
    assert_eq!(
        text(&start, "path"),
        Some(final_path.display().to_string()),
        "the start names where the file goes"
    );
    assert_eq!(done.outcome, Outcome::Ok);
    assert_eq!(text(&done, "path"), report.remote_path);
    assert_eq!(int(&done, "len"), Some(3));
    assert_eq!(
        done.digests.get("sha256").map(String::as_str),
        Some(ABC_SHA256)
    );

    // The customer sees the file arrive from the technician, with its path
    // and size.
    let received = loop {
        let shown = next_shown(&mut live.joiner).await;
        if shown.kind == ActivityKind::FileReceived {
            break shown;
        }
    };
    assert_eq!(received.origin, Origin::Console);
    assert_eq!(received.path, Some(final_path.display().to_string()));
    assert_eq!(received.bytes, Some(3));
    assert_eq!(received.outcome, ActionOutcome::Succeeded);

    // Nothing was staged: the file is the customer's to keep.
    assert!(
        live.service
            .list_staged(None)
            .await
            .expect("list the staged files")
            .is_empty()
    );

    // A file of that name is not replaced unless the technician asks for it.
    let other = local_file(&dir, "other.txt", b"another file");
    let refused = files
        .send_to_folder(
            send(other.clone(), &folder, "abc.txt", false),
            CancelToken::new(),
            None,
        )
        .await
        .expect("a refused transfer is a report");
    assert_eq!(refused.outcome, JobOutcome::Failed);
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(ErrorCode::AlreadyExists)
    );
    assert_eq!(
        std::fs::read(&final_path).expect("read"),
        b"abc",
        "untouched"
    );
    assert_eq!(names(), ["abc.txt"]);

    let replaced = files
        .send_to_folder(
            send(other, &folder, "abc.txt", true),
            CancelToken::new(),
            None,
        )
        .await
        .expect("the transfer runs");
    assert_eq!(replaced.outcome, JobOutcome::Completed);
    assert_eq!(std::fs::read(&final_path).expect("read"), b"another file");
    assert_eq!(names(), ["abc.txt"]);

    let outcomes: Vec<Outcome> = trail
        .of("console_send")
        .iter()
        .map(|entry| entry.outcome)
        .collect();
    assert_eq!(outcomes, [Outcome::Ok, Outcome::Failed, Outcome::Ok]);
    let asked_to_replace: Vec<bool> = trail
        .of("console_send_start")
        .iter()
        .map(|entry| entry.args.0.contains_key("overwrite"))
        .collect();
    assert_eq!(asked_to_replace, [false, false, true]);

    live.service.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file that spans many chunks is sent into a folder and arrives whole; a
/// transfer the technician cancels on the way leaves neither the file nor a
/// part of it in the folder. Windows only, like the test above.
#[cfg(windows)]
#[tokio::test]
async fn a_large_file_arrives_whole_and_a_cancelled_one_leaves_nothing() {
    let live = live_session(explorer_caps(), None).await;
    let trail = Trail::default();
    let files = TechnicianFiles::new(&live.service, &trail);
    let dir = temp_dir_for("send-large");
    let downloads = dir.join("Downloads");
    std::fs::create_dir_all(&downloads).expect("create the folder");
    let folder = downloads.display().to_string();
    let payload: Vec<u8> = (0..3_000_000u32).map(|n| (n % 251) as u8).collect();
    let source = local_file(&dir, "setup.bin", &payload);

    let report = files
        .send_to_folder(
            send(source.clone(), &folder, "setup.bin", false),
            CancelToken::new(),
            None,
        )
        .await
        .expect("the transfer runs");
    assert_eq!(report.outcome, JobOutcome::Completed, "{:?}", report.error);
    assert_eq!(report.len, payload.len() as u64);
    assert_eq!(
        std::fs::read(downloads.join("setup.bin")).expect("read"),
        payload
    );
    assert_eq!(entries_in(&downloads), 1, "no part is left beside the file");

    // Cancelled as soon as the first bytes are confirmed.
    let cancel = CancelToken::new();
    let sink: ProgressSink<cretlink_session::TransferProgress> = {
        let cancel = cancel.clone();
        Box::new(move |_| cancel.cancel())
    };
    let cancelled = files
        .send_to_folder(
            send(source, &folder, "second.bin", false),
            cancel,
            Some(sink),
        )
        .await
        .expect("a cancelled transfer is a report");
    assert_eq!(cancelled.outcome, JobOutcome::Cancelled);
    assert!(!downloads.join("second.bin").exists());
    assert_eq!(
        entries_in(&downloads),
        1,
        "the unfinished part was discarded"
    );
    let outcomes: Vec<Outcome> = trail
        .of("console_send")
        .iter()
        .map(|entry| entry.outcome)
        .collect();
    assert_eq!(outcomes, [Outcome::Ok, Outcome::Cancelled]);

    live.service.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The technician's file actions land in the session's audit file with the
/// session's other records: the sealed chain verifies, the Audit tab lists
/// them as the technician's in the feed's words, and the history counts them
/// as technician actions with the failed one as a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_file_actions_are_in_the_sealed_audit_log_and_the_history() {
    let dir = temp_dir_for("sealed");
    let config = AuditServiceConfig {
        dir: dir.clone(),
        retention: RetentionPolicy::default(),
        soft_cap_bytes: 0,
    };
    let (audit, audit_rx) = ChannelAudit::bounded(1024);
    let (start_tx, start_rx) = oneshot::channel();
    let audit_task = spawn_audit_service(config.clone(), audit_rx, start_rx);

    let live = live_session(explorer_caps(), Some(Arc::new(audit.clone()))).await;
    let log = open_session_log(&config, live.session).expect("open session log");
    start_tx.send(log).expect("deliver session log");

    let folder = staged_folder(live.session, "old-setup");
    plant_files(&folder, 4);
    {
        let files = TechnicianFiles::new(&live.service, &audit);
        files.list_drives().await.expect("the drives are listed");
        let report = files
            .remove_tree(staged("old-setup"), CancelToken::new(), None)
            .await
            .expect("the removal runs");
        assert_eq!(report.outcome, JobOutcome::Completed);
        let missing = files
            .remove_tree(staged("never-there"), CancelToken::new(), None)
            .await
            .expect("a refused removal is a report");
        assert_eq!(missing.outcome, JobOutcome::Failed);
    }

    // End the session so every audit sender drops and the log seals.
    let Live {
        service,
        joiner,
        session,
    } = live;
    service.stop();
    drop(service);
    drop(joiner);
    drop(audit);
    timeout(SEAL, audit_task)
        .await
        .expect("audit drain ends")
        .expect("audit drain joins");

    let mut logs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read audit dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert_eq!(logs.len(), 1, "exactly one session log expected: {logs:?}");
    let path = logs.remove(0);
    let bytes = std::fs::read(&path).expect("read session log");
    let report = verify_sealed(Cursor::new(&bytes)).expect("verify runs");
    assert!(
        matches!(report, ChainReport::Valid { .. }),
        "sealed log must verify: {report:?}"
    );
    let records: Vec<AuditRecord> = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("well-formed record"))
        .collect();

    // Start before completion, both the technician's, sharing the action id.
    let of = |action: &str| -> Vec<&AuditRecord> {
        records
            .iter()
            .filter(|record| record.action == action)
            .collect()
    };
    let id = |record: &AuditRecord| match record.args.0.get("action_id") {
        Some(AuditValue::Int(id)) => *id,
        other => panic!("{} lacks an action id: {other:?}", record.action),
    };
    let (starts, ends) = (of("console_remove_tree_start"), of("console_remove_tree"));
    assert_eq!((starts.len(), ends.len()), (2, 2));
    for (start, end) in starts.iter().zip(&ends) {
        assert_eq!(start.actor, Actor::Console);
        assert_eq!(end.actor, Actor::Console);
        assert_eq!(id(start), id(end));
        assert!(start.seq < end.seq);
        assert_eq!(start.session, session);
        assert_eq!(end.session, session);
    }
    assert_ne!(id(starts[0]), id(starts[1]), "each action has its own id");
    assert_eq!(ends[0].outcome, Outcome::Ok);
    assert_eq!(ends[1].outcome, Outcome::Failed);
    assert_eq!(of("console_fs_drives").len(), 1);

    // The Audit tab lists them as the technician's, in the feed's words.
    let entries = session_entries(&path);
    let removal = entries
        .iter()
        .find(|entry| entry.action == "console_remove_tree")
        .expect("the removal is listed");
    assert_eq!(removal.actor, AuditActorView::Technician);
    assert_eq!(removal.outcome, Outcome::Ok);
    assert_eq!(removal.summary, removal_of("old-setup"));
    let drives = entries
        .iter()
        .find(|entry| entry.action == "console_fs_drives")
        .expect("the drive list is listed");
    assert_eq!(drives.summary, "list the drives");

    // The history counts three technician actions, one of them failed.
    let summary = read_session(&path);
    assert_eq!(summary.actions.technician, 3);
    assert_eq!(summary.actions.ai, 0);
    assert_eq!(summary.failures, 1);
    assert_eq!(
        summary.first_failure.as_deref(),
        Some(removal_of("never-there").as_str())
    );

    let _ = std::fs::remove_dir_all(&dir);
}
