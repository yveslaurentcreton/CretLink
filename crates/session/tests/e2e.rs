//! End-to-end session-runtime tests over **real localhost TCP** (ST-M002-005,
//! ST-M002-006).
//!
//! Each test binds the console to `127.0.0.1:0`, reads the OS-assigned port, and
//! dials it with the agent, so the whole pairing + consent + revoke + cleanup
//! lifecycle runs over a genuine `tokio` TCP connection rather than an in-memory
//! duplex.

// This is a test binary: `unwrap`/`expect`/`panic` are the standard assertion
// style. `allow-*-in-tests` only reaches `#[test]` bodies, not the shared
// driver/assertion helpers below, so allow them for the whole test crate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cretlink_exec::FileStore;
use cretlink_protocol::{
    B64Bytes, CleanupPolicy, DigestAlgo, ErrorCode, OpResult, Operation, PROTOCOL_V1, RemotePath,
    TargetIdentity, canonical_capabilities,
};
use cretlink_session::ConsoleHints;
use cretlink_session::{
    ArmOutcome, CAP_DESKTOP_V1, CAP_REBOOT_RESUME_V1, CAP_TERM_V1, CancelToken,
    DesktopCaptureRequest, DisclosureSummary, DownloadRequest, EndedBy, ExecRequest, HostHandle,
    ImageFormat, JobId, JobOutcome, JoinHandle, NoAudit, OutputStream, PointerAction,
    PointerButton, RebootResumeArmError, RebootResumeArmer, RebootResumeTicketId, RevokeReason,
    SessionAudit, SessionEnd, SessionError, SessionEvent, SessionHost, SessionId, SessionJoiner,
    Shell, TransferProgress, TransferReport, UploadRequest,
};
use cretlink_shared::{Actor, Outcome};
use tokio::time::timeout;

/// Per-event wait budget: localhost pairing/consent is sub-millisecond, so this
/// only guards against a hang.
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Awaits the next lifecycle event on `$handle`, failing on a stream end or a
/// timeout so a stalled runtime never hangs the suite.
macro_rules! next_event {
    ($handle:expr) => {
        timeout(STEP_TIMEOUT, $handle.next_event())
            .await
            .expect("timed out waiting for a session event")
            .expect("session event stream ended unexpectedly")
    };
}

/// Reads events until the stream closes (the task has fully torn down and
/// dropped its senders), so a following command-send deterministically fails.
macro_rules! drain_until_end {
    ($handle:expr) => {{
        loop {
            match timeout(STEP_TIMEOUT, $handle.next_event()).await {
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => panic!("timed out draining the session event stream"),
            }
        }
    }};
}

/// Asserts the next event is a terminal [`SessionEvent::Revoked`] and yields its
/// reason.
macro_rules! expect_revoked {
    ($handle:expr) => {
        match next_event!($handle) {
            SessionEvent::Revoked { reason } => reason,
            other => panic!("expected a Revoked event, got {other:?}"),
        }
    };
}

/// Drains a handle's event stream to its end, returning the reason of a terminal
/// [`SessionEvent::Revoked`] if one arrived, or `None` if the stream closed
/// without one. Unlike [`expect_revoked`] it does not assume the Revoked event
/// is next, so it tolerates a racing teardown where either side's revoke may win
/// and other events may precede the terminal one — and it always drains to the
/// close, so a following command-send deterministically fails closed.
macro_rules! drain_until_revoked {
    ($handle:expr) => {{
        let mut reason = None;
        loop {
            match timeout(STEP_TIMEOUT, $handle.next_event()).await {
                Ok(Some(SessionEvent::Revoked { reason: r })) => reason = Some(r),
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => panic!("timed out draining for a terminal Revoked event"),
            }
        }
        reason
    }};
}

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn console_hints() -> ConsoleHints {
    ConsoleHints {
        label: "This computer (technician)".to_string(),
        display_name: cretlink_protocol::DisplayName::new("Test Technician").ok(),
    }
}

/// The honest identity the agent binds into its consent grant.
fn agent_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "CUSTOMER-PC".to_string(),
        os_version: "Windows 11 Pro".to_string(),
        arch: "x86_64".to_string(),
        elevated: true,
        simulated: false,
        agent_version: "0.0.0".to_string(),
        account: None,
    }
}

fn caps() -> Vec<String> {
    vec!["exec".to_string(), "files".to_string()]
}

/// Binds a console on loopback and dials it with an agent, returning both live
/// handles before either has finished pairing.
async fn connected_pair() -> (HostHandle, JoinHandle) {
    let host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps())
        .await
        .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join(addr, agent_identity(), PROTOCOL_V1, caps())
        .await
        .expect("dial agent");
    (host, joiner)
}

/// Drives both handles through `Connected`, the approval-code compare gate, and
/// `AwaitingConsent`.
///
/// The compare gate (REQ-S-006, ST-M002-013) pauses both endpoints after
/// pairing; the technician (here, the test) releases it by confirming the codes
/// match via [`HostHandle::confirm_code`]. Only then does either side reach
/// `AwaitingConsent`.
async fn drive_to_awaiting_consent(host: &mut HostHandle, joiner: &mut JoinHandle) {
    assert_eq!(next_event!(host), SessionEvent::Connected);
    assert_eq!(next_event!(joiner), SessionEvent::Connected);
    assert_eq!(next_event!(host), SessionEvent::AwaitingApprovalCompare);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingApprovalCompare);
    host.confirm_code().expect("confirm the approval code");
    assert_eq!(next_event!(host), SessionEvent::AwaitingConsent);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingConsent);
}

/// Drives both handles all the way to an active, consented session.
async fn drive_to_active(host: &mut HostHandle, joiner: &mut JoinHandle) {
    drive_to_awaiting_consent(host, joiner).await;
    joiner.grant_consent().expect("grant consent");
    assert_consent_granted(next_event!(host));
    assert_eq!(next_event!(host), SessionEvent::Active);
    assert_consent_granted(next_event!(joiner));
    assert_eq!(next_event!(joiner), SessionEvent::Active);
}

fn assert_consent_granted(event: SessionEvent) {
    match event {
        SessionEvent::ConsentGranted { identity } => assert_eq!(identity, agent_identity()),
        other => panic!("expected ConsentGranted with the honest identity, got {other:?}"),
    }
}

/// Polls `predicate` until it holds or a deadline elapses.
async fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition was not met within the time budget"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// --- happy path: same approval code, consent, active, heartbeats ---

#[tokio::test]
async fn happy_path_same_code_consent_and_heartbeats() {
    let (mut host, mut joiner) = connected_pair().await;

    // Both endpoints reach Connected and derive the SAME six-digit code.
    assert_eq!(next_event!(host), SessionEvent::Connected);
    assert_eq!(next_event!(joiner), SessionEvent::Connected);
    let host_code = host.approval_code().expect("host approval code");
    let joiner_code = joiner.approval_code().expect("joiner approval code");
    assert_eq!(host_code, joiner_code, "endpoints must agree on the SAC");
    assert_eq!(host_code.to_string().len(), 6, "the SAC is six digits");

    // Both endpoints pause at the compare gate with the SAC on screen; no
    // control yet, and consent is not even offered until the technician confirms
    // the codes match (REQ-S-006, ST-M002-013).
    assert_eq!(next_event!(host), SessionEvent::AwaitingApprovalCompare);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingApprovalCompare);
    assert!(!host.control_active());
    assert!(!joiner.control_active());
    host.confirm_code().expect("confirm the approval code");

    // Consent is asked before it is granted; still no control yet.
    assert_eq!(next_event!(host), SessionEvent::AwaitingConsent);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingConsent);
    assert!(!host.control_active());
    assert!(!joiner.control_active());

    // The customer grants; the console observes the honest identity, both reach
    // Active.
    joiner.grant_consent().expect("grant consent");
    assert_consent_granted(next_event!(host));
    assert_eq!(next_event!(host), SessionEvent::Active);
    assert_consent_granted(next_event!(joiner));
    assert_eq!(next_event!(joiner), SessionEvent::Active);
    assert!(host.control_active());
    assert!(joiner.control_active());

    // Both endpoints expose the SAME negotiated session id once control is
    // active (REQ-F-016): the agent's `session()` accessor mirrors the host's,
    // so the agent UI driver can bind its own state to the console's session.
    let host_session = host.session().expect("host session id once active");
    assert_eq!(
        joiner.session(),
        Some(host_session),
        "both endpoints bind the same session id"
    );

    // At least one heartbeat is exchanged in each direction.
    wait_until(|| host.heartbeats_received() >= 1 && joiner.heartbeats_received() >= 1).await;

    host.stop().expect("stop");
}

// --- no control before consent ---

#[tokio::test]
async fn no_control_before_consent() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_awaiting_consent(&mut host, &mut joiner).await;

    // The console received (and can present) the exact disclosure the customer
    // was shown, verbatim from the agent (REQ-S-003, REQ-S-016).
    let shown = host
        .received_disclosure()
        .expect("console must surface the received disclosure once awaiting consent");
    assert_eq!(
        &shown,
        joiner.disclosure(),
        "console must present exactly what the customer saw"
    );

    // Neither side reports control, and the console stays in AwaitingConsent:
    // no Active (or any) event arrives while consent is pending.
    assert!(!host.control_active());
    assert!(!joiner.control_active());
    let pending = timeout(Duration::from_millis(300), host.next_event()).await;
    assert!(
        pending.is_err(),
        "console must stay AwaitingConsent, but emitted {pending:?}"
    );
    assert!(!host.control_active());

    host.stop().expect("stop");
}

// --- consent denied ---

#[tokio::test]
async fn consent_denied_ends_without_active() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_awaiting_consent(&mut host, &mut joiner).await;

    joiner.deny_consent("customer declined").expect("deny");

    // Both end at the consent stage without ever reaching Active.
    match next_event!(joiner) {
        SessionEvent::Failed { stage, .. } => assert_eq!(stage, "consent"),
        other => panic!("expected Failed(consent) on the agent, got {other:?}"),
    }
    match next_event!(host) {
        SessionEvent::Failed { stage, .. } => assert_eq!(stage, "consent"),
        other => panic!("expected Failed(consent) on the console, got {other:?}"),
    }
    assert!(!host.control_active());
    assert!(!joiner.control_active());
}

// --- compare gate: mismatch fails closed, never offers consent ---

/// The whole point of the compare gate: if the technician reports at the compare
/// step that the two Session Approval Codes do NOT match — the signature of a
/// man-in-the-middle — the session tears down fail-closed on both endpoints and
/// consent is never even offered, so control cannot exist (REQ-S-006, REQ-S-009,
/// ST-M002-013).
#[tokio::test]
async fn compare_mismatch_fails_closed_and_grants_no_control() {
    let (mut host, mut joiner) = connected_pair().await;

    // Both endpoints pair and pause at the compare gate with the SAC on screen.
    assert_eq!(next_event!(host), SessionEvent::Connected);
    assert_eq!(next_event!(joiner), SessionEvent::Connected);
    assert_eq!(next_event!(host), SessionEvent::AwaitingApprovalCompare);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingApprovalCompare);

    // The technician declares a mismatch instead of confirming.
    host.report_code_mismatch().expect("report the mismatch");

    // The console fails closed at the compare stage; the agent is revoked over
    // the wire so the customer's screen tears down too rather than hanging on the
    // AWAITING screen.
    match next_event!(host) {
        SessionEvent::Failed { stage, .. } => assert_eq!(stage, "compare"),
        other => panic!("expected Failed(compare) on the console, got {other:?}"),
    }
    assert_eq!(
        drain_until_revoked!(joiner),
        Some(RevokeReason::TechnicianRequested),
        "the agent tears down fail-closed on a compare mismatch"
    );

    // Consent was never offered and control never existed on either endpoint.
    assert!(!host.control_active());
    assert!(!joiner.control_active());
}

// --- compare gate: a technician Stop during the compare window fails closed ---

/// A technician Stop while the codes are still being compared — before any
/// consent — must tear the session down fail-closed on both endpoints and grant
/// no control (REQ-S-009, ST-M002-013).
#[tokio::test]
async fn revoke_during_compare_fails_closed() {
    let (mut host, mut joiner) = connected_pair().await;

    assert_eq!(next_event!(host), SessionEvent::Connected);
    assert_eq!(next_event!(joiner), SessionEvent::Connected);
    assert_eq!(next_event!(host), SessionEvent::AwaitingApprovalCompare);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingApprovalCompare);

    // The technician stops at the compare gate instead of confirming.
    host.revoke(RevokeReason::TechnicianRequested)
        .expect("revoke during compare");

    // Both sides reach a terminal Revoked; neither ever held control.
    assert_eq!(
        drain_until_revoked!(host),
        Some(RevokeReason::TechnicianRequested)
    );
    assert_eq!(
        drain_until_revoked!(joiner),
        Some(RevokeReason::TechnicianRequested),
        "the agent fails closed on a technician Stop during compare"
    );
    assert!(!host.control_active());
    assert!(!joiner.control_active());
}

// --- bilateral revoke: console side ---

#[tokio::test]
async fn console_revoke_tears_down_both_fail_closed() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;
    let old_addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");

    host.revoke(RevokeReason::TechnicianRequested)
        .expect("revoke");
    assert_eq!(expect_revoked!(host), RevokeReason::TechnicianRequested);
    assert_eq!(expect_revoked!(joiner), RevokeReason::TechnicianRequested);
    assert!(!host.control_active());
    assert!(!joiner.control_active());

    drain_until_end!(host);
    drain_until_end!(joiner);

    // Fail-closed: post-teardown actions are refused (the channel is terminal).
    assert!(host.revoke(RevokeReason::TechnicianRequested).is_err());
    assert!(host.stop().is_err());
    assert!(joiner.grant_consent().is_err());
    assert!(joiner.revoke(RevokeReason::CustomerRequested).is_err());

    // A fresh join cannot resume the old session: only one connection is ever
    // accepted, and there is no resume path (REQ-S-009).
    let resumed = SessionJoiner::join(old_addr, agent_identity(), PROTOCOL_V1, caps()).await;
    assert!(
        resumed.is_err(),
        "a fresh join must not reach the closed session"
    );
}

// --- bilateral revoke: agent side ---

#[tokio::test]
async fn agent_revoke_tears_down_both_fail_closed() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    joiner
        .revoke(RevokeReason::CustomerRequested)
        .expect("revoke");
    assert_eq!(expect_revoked!(joiner), RevokeReason::CustomerRequested);
    assert_eq!(expect_revoked!(host), RevokeReason::CustomerRequested);
    assert!(!host.control_active());
    assert!(!joiner.control_active());

    drain_until_end!(joiner);
    drain_until_end!(host);

    assert!(joiner.revoke(RevokeReason::CustomerRequested).is_err());
    assert!(joiner.grant_consent().is_err());
    assert!(host.revoke(RevokeReason::TechnicianRequested).is_err());
}

// --- disconnect mid-session ---

#[tokio::test]
async fn disconnect_mid_session_is_honest_never_success() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;
    wait_until(|| host.heartbeats_received() >= 1).await;

    // Dropping the agent handle aborts its task, closing the TCP socket with no
    // Revoke frame — a hard disconnect.
    drop(joiner);

    // The console surfaces an honest terminal outcome, never a silent success.
    match next_event!(host) {
        SessionEvent::Revoked { reason } => assert_eq!(reason, RevokeReason::Error),
        other => panic!("a disconnect must not read as success; got {other:?}"),
    }
    assert!(!host.control_active());

    drain_until_end!(host);
    let report = host
        .cleanup_report()
        .expect("cleanup report after disconnect");
    // Local teardown still completes even though the peer vanished.
    assert!(report.ok);
    assert!(!report.notes.is_empty());
}

// --- reuse after closure: the agent run loop ends after its one session ---

/// The single-session agent's run loop **ends after its one session**, and the
/// console accepts no second connection (REQ-S-007, REQ-S-009, REQ-S-012). This
/// is the automated proof of the ST-M003-007 "the agent process ends after its
/// session" sub-claim.
///
/// The support-agent binary's `run()` returns exactly when its `JoinHandle`
/// event stream closes: there is no accept loop on the agent side, so when the
/// one `join_task` returns it drops its event sender, `next_event()` yields
/// `None`, `run()` returns, and `main` maps that to an `ExitCode` — the process
/// exits (`apps/support-agent/src/main.rs`). Draining the agent handle to its
/// close is therefore the library-level observation that the agent task ran to
/// completion. A fresh join to the same console address then proves no second
/// session can reuse the closed one; a new session requires a brand-new pairing
/// plus visible consent.
#[tokio::test]
async fn agent_run_loop_ends_after_a_single_session() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;
    let old_addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");

    // A clean technician stop ends the one session.
    host.stop().expect("stop");

    // The agent's event stream closes: its `join_task` returned and dropped its
    // senders. This is the exact condition on which the binary `run()` returns
    // and the agent process exits — the agent never loops to accept another
    // session.
    drain_until_end!(joiner);

    // Fail-closed: the ended agent side refuses any further action.
    assert!(joiner.grant_consent().is_err());
    assert!(joiner.revoke(RevokeReason::CustomerRequested).is_err());
    assert!(!joiner.control_active());

    // The console likewise tears down; draining it confirms it is done too.
    drain_until_end!(host);

    // No second session can reuse the closed console: only one connection is ever
    // accepted and there is no resume path (REQ-S-007, REQ-S-009). The listener
    // was freed after its single accept, so a fresh join cannot reach it.
    let resumed = SessionJoiner::join(old_addr, agent_identity(), PROTOCOL_V1, caps()).await;
    assert!(
        resumed.is_err(),
        "a fresh join must not reach the ended single session"
    );
}

// --- cleanup after stop ---

#[tokio::test]
async fn cleanup_report_is_produced_after_stop() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    host.stop().expect("stop");
    drain_until_end!(host);
    let host_report = host.cleanup_report().expect("host cleanup report");
    assert!(host_report.ok, "clean stop reports ok cleanup (REQ-F-019)");
    assert!(!host_report.notes.is_empty());

    drain_until_end!(joiner);
    let joiner_report = joiner.cleanup_report().expect("agent cleanup report");
    assert!(joiner_report.ok);
}

// --- end records and the agent's cleanup report (ST-M009-012) ---

/// A console-side audit sink keeping the consent-bound identity and the end
/// record the runtime reports.
#[derive(Debug, Default)]
struct EndAudit {
    targets: std::sync::Mutex<Vec<TargetIdentity>>,
    ends: std::sync::Mutex<Vec<SessionEnd>>,
}

impl SessionAudit for EndAudit {
    fn record(&self, _actor: Actor, _action: &'static str, _outcome: Outcome) {}

    fn record_target(&self, identity: &TargetIdentity) {
        self.targets.lock().unwrap().push(identity.clone());
    }

    fn record_end(&self, end: &SessionEnd) {
        self.ends.lock().unwrap().push(end.clone());
    }
}

/// Like [`connected_pair`], with the console's lifecycle recorded into `audit`.
async fn connected_pair_end_audited(audit: Arc<EndAudit>) -> (HostHandle, JoinHandle) {
    let host =
        SessionHost::host_with_audit(loopback(), console_hints(), PROTOCOL_V1, caps(), audit)
            .await
            .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join(addr, agent_identity(), PROTOCOL_V1, caps())
        .await
        .expect("dial agent");
    (host, joiner)
}

#[tokio::test]
async fn technician_stop_records_the_agents_cleanup_report() {
    let audit = Arc::new(EndAudit::default());
    let (mut host, mut joiner) = connected_pair_end_audited(Arc::clone(&audit)).await;
    drive_to_active(&mut host, &mut joiner).await;

    host.revoke(RevokeReason::TechnicianRequested)
        .expect("revoke");
    drain_until_end!(host);
    drain_until_end!(joiner);

    // The agent sent its own teardown report before it closed; the console
    // kept it, and it is the agent's report, not the console's.
    let agent_report = joiner.cleanup_report().expect("agent report");
    let received = host
        .peer_cleanup_report()
        .expect("the agent's report reached the console");
    assert_eq!(received, agent_report);
    assert!(received.ok);
    assert!(
        received
            .notes
            .iter()
            .any(|note| note.contains("revoked by the peer"))
    );

    assert_eq!(host.ended_by(), Some(EndedBy::Technician));
    assert_eq!(joiner.ended_by(), Some(EndedBy::Technician));
    assert_eq!(*audit.targets.lock().unwrap(), vec![agent_identity()]);
    let ends = audit.ends.lock().unwrap().clone();
    assert_eq!(ends.len(), 1, "exactly one end record");
    assert_eq!(ends[0].ended_by, EndedBy::Technician);
    assert_eq!(ends[0].action, "revoked");
    assert!(ends[0].control_granted);
    assert_eq!(ends[0].peer_cleanup.as_ref(), Some(&agent_report));
}

#[tokio::test]
async fn customer_stop_records_the_agents_cleanup_report() {
    let audit = Arc::new(EndAudit::default());
    let (mut host, mut joiner) = connected_pair_end_audited(Arc::clone(&audit)).await;
    drive_to_active(&mut host, &mut joiner).await;

    joiner
        .revoke(RevokeReason::CustomerRequested)
        .expect("revoke");
    drain_until_end!(joiner);
    drain_until_end!(host);

    let agent_report = joiner.cleanup_report().expect("agent report");
    assert_eq!(host.peer_cleanup_report(), Some(agent_report.clone()));
    assert_eq!(host.ended_by(), Some(EndedBy::Customer));
    assert_eq!(joiner.ended_by(), Some(EndedBy::Customer));
    let ends = audit.ends.lock().unwrap().clone();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].ended_by, EndedBy::Customer);
    assert_eq!(ends[0].peer_cleanup, Some(agent_report));
}

#[tokio::test]
async fn a_lost_connection_records_no_cleanup_report() {
    let audit = Arc::new(EndAudit::default());
    let (mut host, mut joiner) = connected_pair_end_audited(Arc::clone(&audit)).await;
    drive_to_active(&mut host, &mut joiner).await;

    // A hard disconnect: the agent task is aborted and sends nothing.
    drop(joiner);
    drain_until_end!(host);

    assert_eq!(
        host.peer_cleanup_report(),
        None,
        "never a fabricated report"
    );
    assert_eq!(host.ended_by(), Some(EndedBy::ConnectionLost));
    let ends = audit.ends.lock().unwrap().clone();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].ended_by, EndedBy::ConnectionLost);
    assert!(ends[0].control_granted);
    assert_eq!(ends[0].peer_cleanup, None);
}

#[tokio::test]
async fn a_declined_consent_is_ended_by_the_customer_without_control() {
    let audit = Arc::new(EndAudit::default());
    let (mut host, mut joiner) = connected_pair_end_audited(Arc::clone(&audit)).await;
    drive_to_awaiting_consent(&mut host, &mut joiner).await;

    joiner.deny_consent("not now").expect("deny");
    drain_until_end!(host);
    drain_until_end!(joiner);

    assert_eq!(host.ended_by(), Some(EndedBy::Customer));
    assert!(
        audit.targets.lock().unwrap().is_empty(),
        "no identity was bound"
    );
    let ends = audit.ends.lock().unwrap().clone();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].action, "consent_denied");
    assert!(!ends[0].control_granted);
}

// --- REQ-S-010: never open an internet-reachable listener ---

#[tokio::test]
async fn console_refuses_a_non_private_bind_address() {
    // The unspecified address would bind every interface, including public ones.
    let unspecified = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
    let refused = SessionHost::host(unspecified, console_hints(), PROTOCOL_V1, caps()).await;
    assert!(refused.is_err(), "0.0.0.0 must be refused (REQ-S-010)");
}

// --- disclosure content (REQ-S-003, REQ-S-016) ---

#[test]
fn disclosure_covers_the_full_control_scope() {
    let text = DisclosureSummary::default().text().to_lowercase();
    for needle in [
        "command", "software", "driver", "setting", "file", "restart", "ai",
    ] {
        assert!(text.contains(needle), "disclosure must mention '{needle}'");
    }
}

// --- M003 exec over the active channel (ST-M003-001/002) ---
//
// These run the whole pairing + consent + exec path over real localhost TCP. The
// agent's broker uses the two real shells on Windows and the portable `sh -c`
// fallback elsewhere, so `echo`/`exit`/`sleep` behave the same on every CI OS.

/// A command that runs far longer than any test needs, for cancel/timeout/revoke.
fn long_command() -> ExecRequest {
    #[cfg(windows)]
    {
        ExecRequest::new(Shell::Cmd, "ping -n 30 127.0.0.1 > nul")
    }
    #[cfg(not(windows))]
    {
        ExecRequest::new(Shell::Cmd, "sleep 30")
    }
}

/// The outcome of draining one job to its terminal event on the console side.
struct ExecResult {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    outcome: JobOutcome,
}

/// Drains the console's event stream for `job`, collecting streamed output on
/// both streams until its terminal `ExecFinished`.
async fn collect_exec(host: &mut HostHandle, job: JobId) -> ExecResult {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        match next_event!(host) {
            SessionEvent::ExecOutput {
                job: j,
                stream,
                bytes,
            } if j == job => match stream {
                OutputStream::Stdout => stdout.extend_from_slice(&bytes),
                OutputStream::Stderr => stderr.extend_from_slice(&bytes),
            },
            SessionEvent::ExecFinished {
                job: j,
                exit_code,
                outcome,
            } if j == job => {
                return ExecResult {
                    stdout,
                    stderr,
                    exit_code,
                    outcome,
                };
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn exec_echo_streams_output_and_completes() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let started = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo hello-remote"))
        .await
        .expect("run echo");
    // The start result reports the effective remote working directory the job
    // actually received (ST-M003-003, REQ-F-009): no directory was requested,
    // so it is the agent process's own, never empty and never invented.
    assert!(
        !started.working_dir.is_empty(),
        "the effective working directory must be reported"
    );
    let job = started.job;
    let result = collect_exec(&mut host, job).await;

    assert_eq!(result.outcome, JobOutcome::Completed);
    assert_eq!(result.exit_code, Some(0));
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("hello-remote"),
        "stdout was {:?}",
        String::from_utf8_lossy(&result.stdout)
    );

    host.stop().expect("stop");
}

#[tokio::test]
async fn exec_failing_command_reports_failed_with_exit_code() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "exit 3"))
        .await
        .expect("run failing command")
        .job;
    let result = collect_exec(&mut host, job).await;

    assert_eq!(result.outcome, JobOutcome::Failed);
    assert_eq!(result.exit_code, Some(3));

    host.stop().expect("stop");
}

#[tokio::test]
async fn exec_streams_stderr_separately_from_stdout() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    // `1>&2` sends the text to standard error in both cmd and sh.
    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo boom 1>&2"))
        .await
        .expect("run stderr command")
        .job;
    let result = collect_exec(&mut host, job).await;

    assert_eq!(result.outcome, JobOutcome::Completed);
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("boom"),
        "stderr was {:?}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        result.stdout.is_empty(),
        "stdout should be empty, was {:?}",
        String::from_utf8_lossy(&result.stdout)
    );

    host.stop().expect("stop");
}

#[tokio::test]
async fn exec_cancel_terminates_the_job_as_cancelled() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let job = host
        .run_command(long_command())
        .await
        .expect("run long")
        .job;
    // Let the remote process actually start before cancelling.
    tokio::time::sleep(Duration::from_millis(200)).await;
    host.cancel_job(job).await.expect("cancel");

    let result = collect_exec(&mut host, job).await;
    assert_eq!(result.outcome, JobOutcome::Cancelled);
    // A prompt Cancelled proves the tree was terminated, not left running.

    // The session stays healthy after a cancel: a fresh command still runs.
    let job2 = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo still-here"))
        .await
        .expect("run after cancel")
        .job;
    let after = collect_exec(&mut host, job2).await;
    assert_eq!(after.outcome, JobOutcome::Completed);

    host.stop().expect("stop");
}

#[tokio::test]
async fn exec_timeout_reports_timed_out() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let job = host
        .run_command(long_command().with_timeout_ms(300))
        .await
        .expect("run with timeout")
        .job;
    let result = collect_exec(&mut host, job).await;
    assert_eq!(result.outcome, JobOutcome::TimedOut);

    host.stop().expect("stop");
}

#[tokio::test]
async fn exec_with_wrong_session_is_rejected_and_not_run() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    // A request stamped with a session this channel does not own must be refused
    // with UnknownSession and never executed (REQ-S-012, REQ-S-014).
    let bogus = SessionId::new();
    let error = host
        .run_command_with_session(bogus, ExecRequest::new(Shell::Cmd, "echo should-not-run"))
        .await
        .expect_err("a wrong-session request must be rejected");
    match error {
        SessionError::Remote { code, .. } => assert_eq!(code, ErrorCode::UnknownSession),
        other => panic!("expected an UnknownSession remote error, got {other:?}"),
    }

    // The session is unharmed: a correctly-scoped command still runs.
    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo scoped-ok"))
        .await
        .expect("run scoped command after rejection")
        .job;
    let result = collect_exec(&mut host, job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);
    assert!(String::from_utf8_lossy(&result.stdout).contains("scoped-ok"));

    host.stop().expect("stop");
}

#[tokio::test]
async fn revoke_mid_job_reports_an_honest_outcome_never_success() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let job = host
        .run_command(long_command())
        .await
        .expect("run long")
        .job;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The customer revokes mid-job: the agent cancels the running tree and the
    // console must see a non-success terminal outcome, then the revoke — never a
    // silent Completed (REQ-S-004, REQ-N-008).
    joiner
        .revoke(RevokeReason::CustomerRequested)
        .expect("revoke");

    let mut finished: Option<JobOutcome> = None;
    let mut revoked = false;
    // Loop until the terminal Revoked arrives (reading `revoked` each turn); a
    // stalled runtime is caught by `next_event!`'s timeout rather than hanging.
    while !revoked {
        match next_event!(host) {
            SessionEvent::ExecFinished {
                job: j, outcome, ..
            } if j == job => finished = Some(outcome),
            SessionEvent::Revoked { reason } => {
                assert_eq!(reason, RevokeReason::CustomerRequested);
                revoked = true;
            }
            _ => {}
        }
    }

    assert!(revoked, "the session must end with a Revoked event");
    let outcome = finished.expect("the running job must report an honest terminal outcome");
    assert_ne!(
        outcome,
        JobOutcome::Completed,
        "a revoked mid-flight job must never read as a success"
    );
    assert!(
        matches!(outcome, JobOutcome::Cancelled | JobOutcome::Unknown),
        "expected Cancelled or Unknown, got {outcome:?}"
    );
}

// --- Interactive terminal over the active session (ADR-0013, ST-M003-009) ---
//
// A terminal is a job: the wire binding reuses the broker's spawn/cancel/exit
// paths. The console's same-user IPC attach/stream/detach surface and its
// multi-consumer adversarial suite are tested in
// `apps/console/tests/terminal_ipc_e2e.rs`.

/// The default capabilities plus the interactive-terminal capability, so the
/// agent services `TermOpen` instead of answering `Unsupported` (REQ-N-012).
fn caps_with_terminal() -> Vec<String> {
    let mut capabilities = caps();
    capabilities.push(CAP_TERM_V1.to_string());
    capabilities
}

/// A connected console/agent pair whose agent advertises `CAP_TERM_V1`.
async fn connected_pair_terminal() -> (HostHandle, JoinHandle) {
    let host = SessionHost::host(
        loopback(),
        console_hints(),
        PROTOCOL_V1,
        caps_with_terminal(),
    )
    .await
    .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join(addr, agent_identity(), PROTOCOL_V1, caps_with_terminal())
        .await
        .expect("dial agent");
    (host, joiner)
}

/// An audit sink that records every `(actor, action)` the runtime emits, so a
/// test can assert a refused terminal op writes **no** `term_close` record.
#[derive(Debug, Default)]
struct RecordingAudit {
    records: std::sync::Mutex<Vec<(Actor, &'static str)>>,
}

impl SessionAudit for RecordingAudit {
    fn record(&self, actor: Actor, action: &'static str, _outcome: Outcome) {
        self.records.lock().unwrap().push((actor, action));
    }
}

/// Like [`connected_pair_terminal`], but records the agent-side audit stream into
/// `audit` so a test can inspect exactly which actions the agent attributed.
async fn connected_pair_terminal_audited(audit: Arc<RecordingAudit>) -> (HostHandle, JoinHandle) {
    let host = SessionHost::host(
        loopback(),
        console_hints(),
        PROTOCOL_V1,
        caps_with_terminal(),
    )
    .await
    .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join_with_audit(
        addr,
        agent_identity(),
        PROTOCOL_V1,
        caps_with_terminal(),
        audit,
    )
    .await
    .expect("dial agent");
    (host, joiner)
}

/// An agent that does not advertise `CAP_TERM_V1` refuses `TermOpen` with
/// `Unsupported` before any pseudo-console is spawned, and the session stays
/// healthy afterwards (REQ-N-012, ST-M003-009). Cross-platform: the capability
/// gate is checked before the Windows-only backend.
#[tokio::test]
async fn terminal_open_without_capability_is_unsupported() {
    let (mut host, mut joiner) = connected_pair().await; // caps() omits term.v1
    drive_to_active(&mut host, &mut joiner).await;

    // Every terminal op — open, resize, and close — is gated on the capability
    // and answered Unsupported before any side effect (REQ-N-012).
    let open_err = host
        .open_terminal(Shell::Cmd, 80, 24, None, BTreeMap::new())
        .await
        .expect_err("a terminal must be unsupported without the capability");
    let resize_err = host
        .resize_terminal(JobId(1), 80, 24)
        .await
        .expect_err("resize must be unsupported without the capability");
    let close_err = host
        .close_terminal(JobId(1))
        .await
        .expect_err("close must be unsupported without the capability");
    for error in [open_err, resize_err, close_err] {
        match error {
            SessionError::Remote { code, .. } => assert_eq!(code, ErrorCode::Unsupported),
            other => panic!("expected an Unsupported remote error, got {other:?}"),
        }
    }

    // The session is unharmed: a normal command still runs.
    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo still-fine"))
        .await
        .expect("run after rejection")
        .job;
    let result = collect_exec(&mut host, job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);

    host.stop().expect("stop");
}

// --- ST-M002-008: pairing prologue uses the version-canonical capability set ---

/// Reproduces the reported cross-machine failure: the console CLI serves **no**
/// operations while a Windows-elevated agent serves its **full** host-detected
/// set. Before ST-M002-008 each endpoint hashed its own served set into the
/// pairing prologue, so these two honest peers computed different prologues, the
/// Noise AEAD tag failed, and the session ended with "secure pairing failed"
/// before a log was opened. Both must now pair and reach an active, consented
/// session (REQ-F-005, REQ-S-008, REQ-N-012).
#[tokio::test]
async fn asymmetric_served_capabilities_still_pair() {
    let host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, Vec::new())
        .await
        .expect("bind console serving no operations");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join(
        addr,
        agent_identity(),
        PROTOCOL_V1,
        // The full set a Windows-elevated agent detects: fs + term + reboot + desktop.
        canonical_capabilities(PROTOCOL_V1),
    )
    .await
    .expect("dial agent serving its full host-detected set");
    let (mut host, mut joiner) = (host, joiner);

    // Both derive the SAME code from a successful handshake, proving the
    // prologues matched despite the asymmetric served sets.
    assert_eq!(next_event!(host), SessionEvent::Connected);
    assert_eq!(next_event!(joiner), SessionEvent::Connected);
    assert_eq!(
        host.approval_code().expect("host code"),
        joiner.approval_code().expect("joiner code"),
        "endpoints must agree on the SAC after an honest asymmetric pair"
    );

    // The compare gate holds, is released by the technician, and consent then
    // completes to an active session.
    assert_eq!(next_event!(host), SessionEvent::AwaitingApprovalCompare);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingApprovalCompare);
    host.confirm_code().expect("confirm the approval code");
    assert_eq!(next_event!(host), SessionEvent::AwaitingConsent);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingConsent);
    joiner.grant_consent().expect("grant consent");
    assert_consent_granted(next_event!(host));
    assert_eq!(next_event!(host), SessionEvent::Active);
    assert_consent_granted(next_event!(joiner));
    assert_eq!(next_event!(joiner), SessionEvent::Active);
    assert!(host.control_active());
    assert!(joiner.control_active());

    host.stop().expect("stop");
}

/// The prologue set (version-canonical) and per-operation gating (the agent's
/// served set) are decoupled. Here the console serves nothing and the agent
/// serves only `caps()` — a strict subset of the canonical prologue, which
/// includes `term.v1`. Pairing still succeeds on the canonical prologue, yet a
/// `TermOpen` is answered `Unsupported` because the agent's *served* set omits
/// `term.v1`. If gating had drifted to read the prologue set, this would wrongly
/// succeed (ST-M002-008, REQ-N-012).
#[tokio::test]
async fn canonical_prologue_does_not_widen_per_op_gating() {
    let host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, Vec::new())
        .await
        .expect("bind console serving no operations");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join(addr, agent_identity(), PROTOCOL_V1, caps())
        .await
        .expect("dial agent serving a strict subset of the canonical set");
    let (mut host, mut joiner) = (host, joiner);

    drive_to_active(&mut host, &mut joiner).await;

    // Gating follows the served set (caps() omits term.v1), not the canonical
    // prologue (which includes it).
    let open_err = host
        .open_terminal(Shell::Cmd, 80, 24, None, BTreeMap::new())
        .await
        .expect_err("term.v1 is not in the agent's served set");
    match open_err {
        SessionError::Remote { code, .. } => assert_eq!(code, ErrorCode::Unsupported),
        other => panic!("expected an Unsupported remote error, got {other:?}"),
    }

    host.stop().expect("stop");
}

/// An out-of-bounds terminal resize is rejected locally before anything reaches
/// the wire, with the typed `LimitExceeded` class preserved, and the session
/// stays healthy (ST-M003-009, REQ-N-012). Cross-platform (local validation).
#[tokio::test]
async fn terminal_resize_out_of_bounds_is_rejected_locally() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    for (cols, rows) in [(0u16, 24u16), (5000, 24), (80, 0)] {
        let error = host
            .resize_terminal(JobId(1), cols, rows)
            .await
            .expect_err("an out-of-bounds resize must be refused locally");
        match error {
            SessionError::Invalid { code, .. } => assert_eq!(
                code,
                ErrorCode::LimitExceeded,
                "wrong error class for {cols}x{rows}"
            ),
            other => panic!("expected a local Invalid rejection, got {other:?}"),
        }
    }

    host.stop().expect("stop");
}

/// `TermClose` is terminal-scoped: aimed at a running **piped** Exec job it is
/// refused with an honest error, the Exec job keeps running (a terminal client
/// cannot cancel arbitrary job trees), and no `Actor::Terminal`/`term_close`
/// audit record is written (REQ-S-023, REQ-N-008, ST-M003-009). Cross-platform:
/// a piped job exists on every OS, and the scope check runs before the
/// Windows-only backend.
#[tokio::test]
async fn term_close_on_a_piped_job_is_refused_and_never_cancels_it() {
    let audit = Arc::new(RecordingAudit::default());
    let (mut host, mut joiner) = connected_pair_terminal_audited(Arc::clone(&audit)).await;
    drive_to_active(&mut host, &mut joiner).await;

    // A normal piped Exec job — not a terminal.
    let job = host
        .run_command(long_command())
        .await
        .expect("start a long piped job")
        .job;

    // TermClose must refuse it honestly, not cancel it and not mis-attribute it.
    let error = host
        .close_terminal(job)
        .await
        .expect_err("TermClose must not target a piped job");
    match error {
        SessionError::Remote { code, .. } => assert_eq!(code, ErrorCode::UnknownJob),
        other => panic!("expected an honest UnknownJob refusal, got {other:?}"),
    }

    // The piped job is untouched: a normal cancel still terminates it as
    // Cancelled, proving TermClose left it running.
    host.cancel_job(job)
        .await
        .expect("the piped job survived and cancels normally");
    let result = collect_exec(&mut host, job).await;
    assert_eq!(
        result.outcome,
        JobOutcome::Cancelled,
        "the piped job survived TermClose and was cancelled normally"
    );

    // No terminal was closed, so no Terminal-actor term_close record exists.
    let records = audit.records.lock().unwrap();
    assert!(
        !records
            .iter()
            .any(|&(actor, action)| actor == Actor::Terminal || action == "term_close"),
        "a refused TermClose must not write a term_close audit record, got {records:?}"
    );

    drop(records);
    host.stop().expect("stop");
}

/// Off Windows there is no pseudo-console backend: even with the capability
/// advertised, `TermOpen` answers `Unsupported` honestly rather than degrading
/// to a piped job (REQ-N-008).
#[cfg(not(windows))]
#[tokio::test]
async fn terminal_open_is_unsupported_off_windows() {
    let (mut host, mut joiner) = connected_pair_terminal().await;
    drive_to_active(&mut host, &mut joiner).await;

    let error = host
        .open_terminal(Shell::Cmd, 80, 24, None, BTreeMap::new())
        .await
        .expect_err("no ConPTY backend exists off Windows");
    match error {
        SessionError::Remote { code, .. } => assert_eq!(code, ErrorCode::Unsupported),
        other => panic!("expected an Unsupported remote error, got {other:?}"),
    }

    host.stop().expect("stop");
}

/// A real end-to-end interactive terminal over a genuine loopback session:
/// open a ConPTY terminal, stream a typed command through and see its output in
/// the merged VT stream, resize within bounds, then close — asserting an honest
/// non-success teardown (a terminal is a job, ST-M003-009, REQ-N-008).
#[cfg(windows)]
#[tokio::test]
async fn terminal_conpty_end_to_end() {
    let (mut host, mut joiner) = connected_pair_terminal().await;
    drive_to_active(&mut host, &mut joiner).await;

    let job = host
        .open_terminal(Shell::Cmd, 80, 24, None, BTreeMap::new())
        .await
        .expect("open terminal");

    // Drive the interactive shell: a typed `echo` runs and prints the marker, so
    // it appears in the merged VT stream regardless of the shell banner.
    host.write_stdin(job, b"echo cretlink-terminal-marker\r\n".to_vec())
        .await
        .expect("write stdin to terminal");

    let mut got_output = false;
    let mut saw_marker = false;
    for _ in 0..500 {
        match next_event!(host) {
            SessionEvent::ExecOutput { job: j, bytes, .. } if j == job => {
                got_output = true;
                if String::from_utf8_lossy(&bytes).contains("cretlink-terminal-marker") {
                    saw_marker = true;
                    break;
                }
            }
            SessionEvent::ExecFinished { job: j, .. } if j == job => break,
            _ => {}
        }
    }
    assert!(got_output, "the terminal must stream non-empty VT output");
    assert!(
        saw_marker,
        "the typed command must appear in the merged terminal stream"
    );

    // A resize within bounds succeeds while the terminal is running.
    host.resize_terminal(job, 100, 40)
        .await
        .expect("resize the terminal within bounds");

    // Closing tears the terminal's tree down; its honest exit still arrives and
    // is never a fabricated clean success (REQ-N-008).
    host.close_terminal(job).await.expect("close terminal");
    let result = collect_exec(&mut host, job).await;
    assert_ne!(
        result.outcome,
        JobOutcome::Completed,
        "a closed terminal is not a clean success"
    );
    assert!(
        matches!(result.outcome, JobOutcome::Cancelled | JobOutcome::Unknown),
        "expected Cancelled or Unknown, got {:?}",
        result.outcome
    );

    host.stop().expect("stop");
}

/// A revoke while a terminal is attached tears it down fail-closed and reports
/// an honest, non-success outcome for the terminal job — never a fabricated
/// clean close (REQ-S-004, REQ-N-008). The terminal reuses the job revoke path.
#[cfg(windows)]
#[tokio::test]
async fn revoke_mid_terminal_reports_an_honest_outcome() {
    let (mut host, mut joiner) = connected_pair_terminal().await;
    drive_to_active(&mut host, &mut joiner).await;

    let job = host
        .open_terminal(Shell::Cmd, 80, 24, None, BTreeMap::new())
        .await
        .expect("open terminal");
    tokio::time::sleep(Duration::from_millis(200)).await;

    joiner
        .revoke(RevokeReason::CustomerRequested)
        .expect("revoke");

    let mut finished: Option<JobOutcome> = None;
    let mut revoked = false;
    while !revoked {
        match next_event!(host) {
            SessionEvent::ExecFinished {
                job: j, outcome, ..
            } if j == job => finished = Some(outcome),
            SessionEvent::Revoked { .. } => revoked = true,
            _ => {}
        }
    }

    let outcome = finished.expect("the terminal job must report an honest terminal outcome");
    assert_ne!(
        outcome,
        JobOutcome::Completed,
        "a revoked terminal must never read as a success"
    );
    assert!(
        matches!(outcome, JobOutcome::Cancelled | JobOutcome::Unknown),
        "expected Cancelled or Unknown, got {outcome:?}"
    );
}

// --- Interactive desktop over the active session (ADR-0015, ST-M005-001/002/003) ---
//
// The desktop op-set sits behind the `desktop.v1` capability gate. Off Windows
// the agent's desktop backend is a fail-closed stub, so exactly two outcomes are
// reachable end to end there: (a) the capability is not advertised, and the op
// is refused before the backend; and (b) the capability is advertised, the gate
// opens, and the backend itself answers `DesktopUnsupported`. Windows capture is
// covered by the `#[ignore]`d live test below.

/// The default capabilities plus the interactive-desktop capability, so the
/// agent's runtime opens the gate and forwards desktop ops to its backend
/// instead of answering `Unsupported` for the capability (REQ-N-012).
fn caps_with_desktop() -> Vec<String> {
    let mut capabilities = caps();
    capabilities.push(CAP_DESKTOP_V1.to_string());
    capabilities
}

/// A connected console/agent pair whose agent advertises `CAP_DESKTOP_V1`.
async fn connected_pair_desktop() -> (HostHandle, JoinHandle) {
    let host = SessionHost::host(
        loopback(),
        console_hints(),
        PROTOCOL_V1,
        caps_with_desktop(),
    )
    .await
    .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join(addr, agent_identity(), PROTOCOL_V1, caps_with_desktop())
        .await
        .expect("dial agent");
    (host, joiner)
}

/// An agent that does not advertise `CAP_DESKTOP_V1` refuses every desktop
/// operation — capture, pointer input, and clipboard read — with `Unsupported`
/// before any side effect, naming the missing capability, and the session stays
/// healthy afterwards (REQ-N-012, ADR-0015). Cross-platform: the gate is checked
/// before any backend is consulted.
#[tokio::test]
async fn desktop_without_capability_is_unsupported() {
    let (mut host, mut joiner) = connected_pair().await; // caps() omits desktop.v1
    drive_to_active(&mut host, &mut joiner).await;

    let capture_err = host
        .capture_desktop(DesktopCaptureRequest::new(0, ImageFormat::Png))
        .await
        .expect_err("capture must be unsupported without the capability");
    let pointer_err = host
        .desktop_pointer(PointerAction::Move, PointerButton::Left, 10, 10)
        .await
        .expect_err("pointer input must be unsupported without the capability");
    let clipboard_err = host
        .clipboard_get()
        .await
        .expect_err("clipboard read must be unsupported without the capability");

    for error in [capture_err, pointer_err, clipboard_err] {
        match error {
            SessionError::Remote { code, message } => {
                assert_eq!(code, ErrorCode::Unsupported);
                assert!(
                    message.contains("not advertised"),
                    "a refusal before the gate must name the missing capability, got {message:?}"
                );
            }
            other => panic!("expected an Unsupported remote error, got {other:?}"),
        }
    }

    // The session is unharmed: a normal command still runs.
    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo still-fine"))
        .await
        .expect("run after rejection")
        .job;
    let result = collect_exec(&mut host, job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);

    host.stop().expect("stop");
}

/// An agent that advertises `CAP_DESKTOP_V1` on a platform **without** a
/// capture-and-input backend opens the gate, forwards the op, and then answers
/// `Unsupported` **from the backend** — never a fabricated frame or a silently
/// dropped input (REQ-N-008, REQ-S-009). This drives the enabled dispatch path
/// the ungated test cannot reach; the distinct backend message proves the op
/// passed the gate.
///
/// It is `cfg(not(windows))` because the Windows build carries a real backend
/// (ST-M005-001): there the same ops would capture the live screen and
/// synthesize a real keystroke into the test runner's desktop, so the Windows
/// path is exercised by the `#[ignore]`d live smoke test below and by manual
/// testing on hardware instead. The refusal-before-the-gate behaviour is proven
/// on every platform by `desktop_wait_is_serviced_locally_under_the_capability`.
#[cfg(not(windows))]
#[tokio::test]
async fn desktop_enabled_but_backend_is_unsupported() {
    use cretlink_session::KeyAction;

    let (mut host, mut joiner) = connected_pair_desktop().await;
    drive_to_active(&mut host, &mut joiner).await;

    let capture_err = host
        .capture_desktop(DesktopCaptureRequest::new(0, ImageFormat::Png))
        .await
        .expect_err("capture must be unsupported without a backend");
    let key_err = host
        .desktop_key(KeyAction::Press, "enter")
        .await
        .expect_err("key input must be unsupported without a backend");

    for error in [capture_err, key_err] {
        match error {
            SessionError::Remote { code, message } => {
                assert_eq!(code, ErrorCode::Unsupported);
                assert!(
                    message.contains("backend is not available"),
                    "an enabled-but-unsupported refusal must come from the backend, got {message:?}"
                );
            }
            other => panic!("expected an Unsupported remote error, got {other:?}"),
        }
    }

    // The session is unharmed: a normal command still runs.
    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo still-fine"))
        .await
        .expect("run after rejection")
        .job;
    let result = collect_exec(&mut host, job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);

    host.stop().expect("stop");
}

/// A `DesktopWait` is session-local pacing, not synthesized input, so it is
/// serviced by the agent's session even where no capture-and-input backend
/// exists — yet it still requires the capability to be advertised (ADR-0015,
/// ST-M005-002). With the gate open the bounded wait completes; without the
/// capability the identical wait is refused like every other desktop op.
/// Cross-platform.
#[tokio::test]
async fn desktop_wait_is_serviced_locally_under_the_capability() {
    let (mut host, mut joiner) = connected_pair_desktop().await;
    drive_to_active(&mut host, &mut joiner).await;
    host.desktop_wait(1)
        .await
        .expect("a bounded wait must be serviced by the session under the capability");
    host.stop().expect("stop");

    let (mut ungated, mut ungated_joiner) = connected_pair().await;
    drive_to_active(&mut ungated, &mut ungated_joiner).await;
    let err = ungated
        .desktop_wait(1)
        .await
        .expect_err("a wait must be unsupported without the capability");
    match err {
        SessionError::Remote { code, message } => {
            assert_eq!(code, ErrorCode::Unsupported);
            assert!(
                message.contains("not advertised"),
                "the ungated wait must be refused before the gate, got {message:?}"
            );
        }
        other => panic!("expected an Unsupported remote error, got {other:?}"),
    }
    ungated.stop().expect("stop");
}

/// A synthesized-input flood is *paced* end-to-end, never dropped
/// (REQ-N-006, ST-M005-002). The session's input governor absorbs a
/// short burst without delay, then holds a sustained ceiling: driving well past
/// the burst allowance forces the run to take at least the governor's pacing
/// floor for the post-burst actions. Every action still reaches the backend (the
/// governor only ever delays — it does not refuse), which is why each call here
/// comes back as an honest backend `Unsupported`, one per action.
///
/// `cfg(not(windows))` for the same reason as `desktop_enabled_but_backend_is_
/// unsupported`: on Windows these calls would synthesize real keystrokes into the
/// test runner's desktop. Off Windows the backend refuses *after* the gate, so
/// the pacing — which runs before the backend — is exactly what this measures.
/// The assertion is a lower bound only (sleeps overshoot, never undershoot), so
/// it cannot flake on a slow or loaded machine.
#[cfg(not(windows))]
#[tokio::test]
async fn a_synthesized_input_flood_is_paced_not_dropped() {
    use cretlink_protocol::limits::{MAX_DESKTOP_INPUT_BURST, MIN_DESKTOP_INPUT_INTERVAL_MS};
    use cretlink_session::KeyAction;

    let (mut host, mut joiner) = connected_pair_desktop().await;
    drive_to_active(&mut host, &mut joiner).await;

    let burst = MAX_DESKTOP_INPUT_BURST as usize;
    let interval = Duration::from_millis(MIN_DESKTOP_INPUT_INTERVAL_MS);
    // Three burst-lengths of actions: the first `burst` fire freely, the rest are
    // paced to one per interval.
    let actions = burst * 3;
    let paced = actions - burst;

    let started = tokio::time::Instant::now();
    for _ in 0..actions {
        // The gate paces first, then the backend refuses (no backend off Windows);
        // an error per call proves nothing was silently dropped before the gate.
        host.desktop_key(KeyAction::Press, "a")
            .await
            .expect_err("input is unsupported without a backend, but still gated");
    }
    let elapsed = started.elapsed();

    // Half the pure pacing sum: a generous, sleep-only lower bound that a runaway
    // flood could never beat, yet loose enough never to flake.
    let floor = interval * (paced as u32) / 2;
    assert!(
        elapsed >= floor,
        "a flood of {actions} actions must be paced to at least {floor:?}, took {elapsed:?}"
    );

    // The session is unharmed and still responsive after the paced flood.
    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo still-fine"))
        .await
        .expect("run after a paced flood")
        .job;
    assert_eq!(
        collect_exec(&mut host, job).await.outcome,
        JobOutcome::Completed
    );
    host.stop().expect("stop");
}

/// Live Windows capture smoke test (ST-M005-001). `#[ignore]` because it
/// only means anything on a machine with a real interactive desktop and it reads
/// the live screen — the maintainer runs it explicitly on hardware
/// (`cargo test -p cretlink-session --test e2e -- --ignored live_windows`). It
/// never synthesizes input, so it cannot disturb the desktop; it only proves the
/// gate opens, the real backend answers, and the frame is honest: an `Active`
/// desktop carries PNG bytes, any other state carries none — never a fabricated
/// frame (REQ-N-008).
#[cfg(windows)]
#[tokio::test]
#[ignore = "reads the live Windows desktop; run explicitly on attended hardware"]
async fn live_windows_capture_is_honest() {
    let (mut host, mut joiner) = connected_pair_desktop().await;
    drive_to_active(&mut host, &mut joiner).await;

    let frame = host
        .capture_desktop(DesktopCaptureRequest::new(0, ImageFormat::Png))
        .await
        .expect("a Windows backend must answer capture, not refuse it");

    if frame.meta.state == cretlink_session::DesktopState::Active {
        let image = frame
            .image
            .expect("an Active desktop must carry a pixel payload");
        assert_eq!(
            &image[..8],
            &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'],
            "an Active frame must be a real PNG"
        );
        assert!(frame.meta.physical_width > 0 && frame.meta.physical_height > 0);
    } else {
        assert!(
            frame.image.is_none(),
            "a non-Active desktop must never carry a fabricated frame, state was {:?}",
            frame.meta.state
        );
    }

    host.stop().expect("stop");
}

/// A network interruption **during** a live desktop session fails closed: once
/// the peer vanishes and the console has surfaced the honest terminal outcome,
/// every desktop operation refuses — never a fabricated frame and never a
/// silently-accepted input — and it refuses *promptly* rather than hanging on a
/// response that will never come (REQ-S-004, REQ-S-009, REQ-N-008).
///
/// This is the desktop analogue of `disconnect_mid_session_is_honest_never_
/// success`: a hard socket drop with no `Revoke` frame. It is cross-platform and
/// backend-independent by construction — the agent task is gone, so no capture
/// or input backend can be consulted on any OS; the op fails because the channel
/// is dead, not because a backend answered. Each op is wrapped in the per-step
/// timeout so a runtime that waited forever for a dead peer would surface as an
/// explicit "must fail closed, not hang" failure instead of stalling the suite.
#[tokio::test]
async fn desktop_op_after_network_interruption_fails_closed() {
    let (mut host, mut joiner) = connected_pair_desktop().await;
    drive_to_active(&mut host, &mut joiner).await;
    wait_until(|| host.heartbeats_received() >= 1).await;

    // Dropping the agent handle aborts its task, closing the TCP socket with no
    // Revoke frame — a hard disconnect mid-session.
    drop(joiner);

    // The console surfaces an honest terminal outcome, never a silent success.
    match next_event!(host) {
        SessionEvent::Revoked { reason } => assert_eq!(reason, RevokeReason::Error),
        other => panic!("a disconnect must not read as success; got {other:?}"),
    }
    assert!(!host.control_active());

    // A capture after the interruption fails closed — never a fabricated frame —
    // and does so promptly (the timeout turns a hang into an explicit failure).
    let capture = timeout(
        STEP_TIMEOUT,
        host.capture_desktop(DesktopCaptureRequest::new(0, ImageFormat::Png)),
    )
    .await
    .expect("capture after a network interruption must fail closed, not hang");
    assert!(
        capture.is_err(),
        "a capture after the peer vanished must never yield a frame; got {capture:?}"
    );

    // An input action after the interruption fails closed too — never silently
    // accepted as delivered.
    let pointer = timeout(
        STEP_TIMEOUT,
        host.desktop_pointer(PointerAction::Move, PointerButton::Left, 10, 10),
    )
    .await
    .expect("input after a network interruption must fail closed, not hang");
    assert!(
        pointer.is_err(),
        "an input action after the peer vanished must never read as delivered; got {pointer:?}"
    );

    // Local teardown still completes honestly even though the peer is gone.
    drain_until_end!(host);
    let report = host
        .cleanup_report()
        .expect("cleanup report after a desktop-session interruption");
    assert!(report.ok);
    assert!(!report.notes.is_empty());
}

// --- M003 target/context inspection over the active channel (ST-M003-003) ---

/// Binds a console and dials it with an agent whose reboot probe is injected
/// deterministically, so a live `SystemInfo` answer is distinguishable from a
/// hardcoded one.
async fn connected_pair_with_probe(reboot_pending: bool) -> (HostHandle, JoinHandle) {
    let host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps())
        .await
        .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join_configured(
        addr,
        agent_identity(),
        PROTOCOL_V1,
        caps(),
        Arc::new(NoAudit),
        Arc::new(move || reboot_pending),
    )
    .await
    .expect("dial agent");
    (host, joiner)
}

/// `SystemInfo` is answered live by the real agent runtime: the honest
/// consent-bound identity (never a placeholder literal) and the injected
/// reboot probe's value (never a hardcoded `false`).
#[tokio::test]
async fn system_info_is_answered_live_by_the_agent() {
    let (mut host, mut joiner) = connected_pair_with_probe(true).await;
    drive_to_active(&mut host, &mut joiner).await;

    let (target, reboot_pending) = host.system_info().await.expect("system_info");
    assert_eq!(target, agent_identity());
    assert_ne!(
        target.os_version, "Windows",
        "the bare literal is never a real Windows version"
    );
    assert!(
        reboot_pending,
        "the live probe's value must be reported, not a hardcoded false"
    );

    host.stop().expect("stop");
}

/// `ListJobs` on the real runtime reports running jobs and the bounded
/// finished history (`running: false`), matching the simulator's behaviour.
#[tokio::test]
async fn list_jobs_reports_running_and_finished_history() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let finished = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo history"))
        .await
        .expect("run echo")
        .job;
    let result = collect_exec(&mut host, finished).await;
    assert_eq!(result.outcome, JobOutcome::Completed);

    let running = host
        .run_command(long_command())
        .await
        .expect("run long")
        .job;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let jobs = host.list_jobs().await.expect("list_jobs");
    assert!(
        jobs.iter().any(|s| s.job == finished && !s.running),
        "the finished job must appear as history with running=false, got {jobs:?}"
    );
    assert!(
        jobs.iter().any(|s| s.job == running && s.running),
        "the running job must be listed, got {jobs:?}"
    );

    host.cancel_job(running).await.expect("cancel");
    let result = collect_exec(&mut host, running).await;
    assert_eq!(result.outcome, JobOutcome::Cancelled);

    host.stop().expect("stop");
}

/// Over the wire, a requested remote working directory is validated, applied
/// (a relative write inside the job lands there), and echoed back as the
/// effective working directory (ST-M003-003, REQ-F-009). Windows-only: the
/// wire schema accepts only drive-qualified remote paths, which exist only on
/// a Windows-style filesystem.
#[cfg(windows)]
#[tokio::test]
async fn exec_working_dir_resolves_relative_access_remotely() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("exec-cwd");
    let mut request = ExecRequest::new(Shell::Cmd, "echo remote-marker> cwd-proof.txt");
    request.working_dir = Some(dir.display().to_string());

    let started = host.run_command(request).await.expect("run in working dir");
    assert_eq!(
        started.working_dir,
        dir.display().to_string(),
        "the effective working directory is the requested remote one"
    );
    let result = collect_exec(&mut host, started.job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);

    let proof = dir.join("cwd-proof.txt");
    let contents = std::fs::read_to_string(&proof)
        .unwrap_or_else(|e| panic!("relative write must land in {}: {e}", proof.display()));
    assert!(contents.contains("remote-marker"));
    let _ = std::fs::remove_dir_all(&dir);

    host.stop().expect("stop");
}

/// An adversarial `working_dir` is rejected locally before anything reaches
/// the wire (the same rules as absolute remote paths, ST-M003-003).
#[tokio::test]
async fn exec_adversarial_working_dir_is_refused_locally() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    for bad in [
        "C:\\Users\\..\\Windows",
        "\\\\server\\share",
        "relative\\dir",
    ] {
        let mut request = ExecRequest::new(Shell::Cmd, "echo should-not-run");
        request.working_dir = Some(bad.to_string());
        let error = host
            .run_command(request)
            .await
            .expect_err("an adversarial working_dir must be refused");
        match error {
            // The typed class survives the local rejection (ST-M003-003):
            // a bad path is invalid_path, never a collapsed limit_exceeded.
            SessionError::Invalid { code, .. } => assert_eq!(
                code,
                ErrorCode::InvalidPath,
                "wrong error class for {bad:?}"
            ),
            other => panic!("expected a local Invalid rejection for {bad:?}, got {other:?}"),
        }
    }

    // The session stays healthy after the local rejections.
    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo still-fine"))
        .await
        .expect("run after rejections")
        .job;
    let result = collect_exec(&mut host, job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);

    host.stop().expect("stop");
}

#[tokio::test]
async fn exec_before_active_is_refused() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_awaiting_consent(&mut host, &mut joiner).await;

    // No control exists before consent: an exec must fail closed, not hang.
    let error = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo nope"))
        .await
        .expect_err("exec before active must be refused");
    assert!(
        matches!(error, SessionError::NotActive),
        "expected NotActive, got {error:?}"
    );

    host.stop().expect("stop");
}

// --- M003 file operations over the active channel (ST-M003-004/005, ADR-0012) ---
//
// These exercise the console's typed `file_op` primitive against the REAL agent
// FileStore over localhost TCP: stage bytes into a session-namespaced part,
// commit them (the agent fsyncs, verifies length and SHA-256, then atomically
// renames), stat and read the verified artifact back, and see it in the staging
// manifest. A clean stop makes the agent sweep the OnSessionEnd artifact.

/// The SHA-256 of `bytes`, as the raw 32-byte digest an `FsCommit` must carry.
fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).to_vec()
}

#[tokio::test]
async fn file_op_stages_commits_and_reads_back_over_tcp() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let name = "e2e-artifact.bin";
    let payload = b"cretlink file-op round-trip over real TCP\n";
    let digest = sha256(payload);
    let staged = || RemotePath::Staging {
        name: name.to_string(),
    };

    // Append the whole payload to the session part; the agent reports the new
    // part length as the next expected write offset.
    match host
        .file_op(Operation::FsWrite {
            dest: staged(),
            offset: 0,
            data: B64Bytes::new(payload.to_vec()),
        })
        .await
        .expect("FsWrite over TCP")
    {
        OpResult::WriteAccepted { part_len } => assert_eq!(part_len, payload.len() as u64),
        other => panic!("expected WriteAccepted, got {other:?}"),
    }

    // Commit: the ONLY wire path to a final filename. The agent verifies length
    // and SHA-256 before the atomic rename, so a corrupt artifact is
    // unrepresentable (REQ-N-007).
    let canonical = match host
        .file_op(Operation::FsCommit {
            dest: staged(),
            len: payload.len() as u64,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(digest.clone()),
            overwrite: false,
            source: Some("e2e".to_string()),
            cleanup: CleanupPolicy::OnSessionEnd,
        })
        .await
        .expect("FsCommit over TCP")
    {
        OpResult::Committed {
            canonical_path,
            len,
            digest: got,
            replaced,
            ..
        } => {
            assert_eq!(len, payload.len() as u64);
            assert_eq!(got.as_bytes(), digest.as_slice());
            assert!(!replaced, "a fresh commit replaces nothing");
            canonical_path
        }
        other => panic!("expected Committed, got {other:?}"),
    };
    assert!(!canonical.is_empty(), "the committed path is reported");

    // Stat the committed artifact: a final entry exists, no pending part remains.
    match host
        .file_op(Operation::FsStat { path: staged() })
        .await
        .expect("FsStat over TCP")
    {
        OpResult::PathStat { entry, partial_len } => {
            let entry = entry.expect("the committed artifact must stat as present");
            assert_eq!(entry.len, payload.len() as u64);
            assert!(partial_len.is_none(), "the part was consumed by the commit");
        }
        other => panic!("expected PathStat, got {other:?}"),
    }

    // Read the verified bytes back and confirm they match exactly.
    match host
        .file_op(Operation::FsRead {
            path: staged(),
            offset: 0,
            len: payload.len() as u32,
        })
        .await
        .expect("FsRead over TCP")
    {
        OpResult::FileData {
            data,
            eof,
            file_len,
        } => {
            assert_eq!(data.as_bytes(), payload);
            assert!(eof, "the whole file was read");
            assert_eq!(file_len, payload.len() as u64);
        }
        other => panic!("expected FileData, got {other:?}"),
    }

    // The artifact appears in the session's staging manifest with its label,
    // verified length, and digest (matched on stable fields, not path spelling).
    match host
        .file_op(Operation::ListStaged {})
        .await
        .expect("ListStaged over TCP")
    {
        OpResult::StagedList { entries } => {
            assert!(
                entries.iter().any(|e| e.source == "e2e"
                    && e.len == payload.len() as u64
                    && e.digest.as_bytes() == digest.as_slice()),
                "the staged artifact must be listed, got {entries:?}"
            );
        }
        other => panic!("expected StagedList, got {other:?}"),
    }

    // A clean stop ends the session; the agent sweeps the OnSessionEnd artifact
    // and reports an honest cleanup (REQ-F-019).
    host.stop().expect("stop");
    drain_until_end!(host);
    let report = host.cleanup_report().expect("host cleanup report");
    assert!(report.ok, "a clean stop reports ok cleanup");
}

#[tokio::test]
async fn file_op_with_wrong_session_is_rejected_and_not_run() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    // A file op stamped with a session this channel does not own must be refused
    // with UnknownSession and never touch the FileStore (REQ-S-012, REQ-S-014).
    let bogus = SessionId::new();
    let error = host
        .file_op_with_session(
            bogus,
            Operation::FsStat {
                path: RemotePath::Staging {
                    name: "should-not-run.bin".to_string(),
                },
            },
        )
        .await
        .expect_err("a wrong-session file op must be rejected");
    match error {
        SessionError::Remote { code, .. } => assert_eq!(code, ErrorCode::UnknownSession),
        other => panic!("expected an UnknownSession remote error, got {other:?}"),
    }

    // The session is unharmed: a correctly-scoped file op still answers.
    match host
        .file_op(Operation::ListStaged {})
        .await
        .expect("ListStaged after rejection")
    {
        OpResult::StagedList { entries } => assert!(entries.is_empty()),
        other => panic!("expected an empty StagedList, got {other:?}"),
    }

    host.stop().expect("stop");
}

/// A deterministic, position-dependent byte pattern of `len` bytes. Position
/// dependence means a wrong-offset write or a mismatched prefix cannot coincide.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// A cleaned, test-private local directory under the OS temp root.
fn temp_dir_for(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cretlink-xfer-{test}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A payload large enough to fill the four-deep in-flight window and force
/// mid-stream backpressure (more than `MAX_INFLIGHT_FILE_OPS` wire chunks), plus
/// a partial tail. Sized in raw bytes: the engine chunks far smaller than
/// `MAX_CHUNK_BYTES` to keep every message inside one secure-channel record.
fn windowed_payload() -> Vec<u8> {
    pattern(200 * 1024 + 100)
}

/// A payload far larger than the four-deep in-flight window can hold, so
/// streaming it needs many wire round trips (at `WIRE_CHUNK_BYTES` = 32 KiB this
/// is 128 chunks). A revoke fired from the first progress callback therefore
/// always lands mid-stream — after real bytes are acknowledged in flight but
/// long before the commit — without a timing sleep (ST-M003-007).
fn large_payload() -> Vec<u8> {
    pattern(4 * 1024 * 1024)
}

/// Asserts a progress trace is monotonic and ends exactly at `total`.
fn assert_progress_reaches(progress: &[TransferProgress], total: u64) {
    assert!(
        progress.len() >= 2,
        "the window must report incremental progress, got {progress:?}"
    );
    let mut last = 0u64;
    for p in progress {
        assert_eq!(
            p.total, total,
            "every progress snapshot carries the true total"
        );
        assert!(
            p.transferred >= last,
            "progress never goes backwards: {progress:?}"
        );
        last = p.transferred;
    }
    assert_eq!(last, total, "progress ends at the full total");
}

#[tokio::test]
async fn upload_then_download_round_trips_over_tcp() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("round-trip");
    let data = windowed_payload();
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write source");
    let name = "round-trip.bin";
    let cancel = CancelToken::new();

    // Upload: pipelined writes, then a verified FsCommit. Nothing is resumed and
    // nothing is restarted on a clean first run.
    let mut up_progress = Vec::new();
    let report = host
        .upload_file(
            UploadRequest {
                source: source.clone(),
                dest: RemotePath::Staging {
                    name: name.to_string(),
                },
                source_label: "e2e-upload".to_string(),
                cleanup: CleanupPolicy::Keep,
                overwrite: false,
            },
            &cancel,
            |p| up_progress.push(p),
        )
        .await
        .expect("upload runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(report.total, data.len() as u64);
    assert_eq!(report.sent, data.len() as u64);
    assert_eq!(report.resumed_from, 0);
    assert!(!report.restarted);
    assert_eq!(
        report.digest,
        sha256(&data),
        "the committed digest is the source digest"
    );
    assert!(
        report.canonical_path.is_some(),
        "a completed upload reports its committed path"
    );
    assert_progress_reaches(&up_progress, data.len() as u64);

    // Download the committed artifact back and confirm the bytes are identical.
    let dest = dir.join("round-trip-download.bin");
    let mut down_progress = Vec::new();
    let report = host
        .download_file(
            DownloadRequest {
                source: RemotePath::Staging {
                    name: name.to_string(),
                },
                dest: dest.clone(),
            },
            &cancel,
            |p| down_progress.push(p),
        )
        .await
        .expect("download runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(report.resumed_from, 0);
    assert!(!report.restarted);
    assert_eq!(report.digest, sha256(&data));
    assert_progress_reaches(&down_progress, data.len() as u64);
    let got = std::fs::read(&dest).expect("read downloaded file");
    assert_eq!(got, data, "the downloaded bytes match the original source");
    assert!(
        !download_temp_exists(&dest),
        "the verified rename consumes the temp"
    );

    host.stop().expect("stop");
    drain_until_end!(host);
}

#[tokio::test]
async fn upload_resumes_from_a_matching_partial() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("upload-resume");
    let data = windowed_payload();
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write source");
    let name = "upload-resume.bin";
    let staged = || RemotePath::Staging {
        name: name.to_string(),
    };

    // Pre-stage a genuine prefix as a pending part (a prior interrupted upload).
    let prefix = 300usize;
    match host
        .file_op(Operation::FsWrite {
            dest: staged(),
            offset: 0,
            data: B64Bytes::new(data[..prefix].to_vec()),
        })
        .await
        .expect("seed a matching part")
    {
        OpResult::WriteAccepted { part_len } => assert_eq!(part_len, prefix as u64),
        other => panic!("expected WriteAccepted, got {other:?}"),
    }

    // The upload verifies the part is a true prefix and resumes from it. A
    // Completed outcome with the full-source digest proves the resume was exact:
    // the agent re-verifies the whole-file digest at commit (REQ-N-007).
    let report = host
        .upload_file(
            UploadRequest {
                source: source.clone(),
                dest: staged(),
                source_label: "e2e-resume".to_string(),
                cleanup: CleanupPolicy::OnSessionEnd,
                overwrite: false,
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("upload runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(
        report.resumed_from, prefix as u64,
        "the matching prefix is reused"
    );
    assert!(!report.restarted, "a matching prefix is not a restart");
    assert_eq!(
        report.sent,
        (data.len() - prefix) as u64,
        "only the tail is sent"
    );
    assert_eq!(report.digest, sha256(&data));

    host.stop().expect("stop");
    drain_until_end!(host);
}

#[tokio::test]
async fn upload_restarts_on_a_prefix_mismatch() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("upload-restart");
    let data = windowed_payload();
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write source");
    let name = "upload-restart.bin";
    let staged = || RemotePath::Staging {
        name: name.to_string(),
    };

    // Pre-stage a part that is NOT a prefix of the source (all 0xFF).
    let prefix = 300usize;
    match host
        .file_op(Operation::FsWrite {
            dest: staged(),
            offset: 0,
            data: B64Bytes::new(vec![0xFF; prefix]),
        })
        .await
        .expect("seed a foreign part")
    {
        OpResult::WriteAccepted { part_len } => assert_eq!(part_len, prefix as u64),
        other => panic!("expected WriteAccepted, got {other:?}"),
    }

    // The foreign part is not a prefix of the source, so the whole-file digest
    // fails at commit; the engine aborts the part and restarts from zero rather
    // than leaving wrong bytes silently committed.
    let report = host
        .upload_file(
            UploadRequest {
                source: source.clone(),
                dest: staged(),
                source_label: "e2e-restart".to_string(),
                cleanup: CleanupPolicy::OnSessionEnd,
                overwrite: false,
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("upload runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    assert!(
        report.restarted,
        "a foreign prefix forces an abort-and-restart"
    );
    assert_eq!(report.resumed_from, 0, "a restart reuses nothing");
    assert_eq!(report.sent, data.len() as u64, "the whole file is resent");
    assert_eq!(report.digest, sha256(&data));

    host.stop().expect("stop");
    drain_until_end!(host);
}

#[tokio::test]
async fn upload_cancelled_before_start_stages_nothing() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("upload-cancel");
    let data = pattern(4096);
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write source");
    let name = "upload-cancel.bin";
    let staged = || RemotePath::Staging {
        name: name.to_string(),
    };

    // A token already cancelled: the upload must not stage or commit anything.
    let cancel = CancelToken::new();
    cancel.cancel();
    let report = host
        .upload_file(
            UploadRequest {
                source: source.clone(),
                dest: staged(),
                source_label: "e2e-cancel".to_string(),
                cleanup: CleanupPolicy::OnSessionEnd,
                overwrite: false,
            },
            &cancel,
            |_p| {},
        )
        .await
        .expect("upload runs");
    assert_eq!(report.outcome, JobOutcome::Cancelled);
    assert_eq!(report.sent, 0, "a pre-cancelled upload sends nothing");
    assert!(report.canonical_path.is_none(), "nothing was committed");

    // Neither a final entry nor a pending part is left behind.
    match host
        .file_op(Operation::FsStat { path: staged() })
        .await
        .expect("FsStat after cancel")
    {
        OpResult::PathStat { entry, partial_len } => {
            assert!(entry.is_none(), "no artifact was committed");
            assert!(partial_len.is_none(), "the cancel left no dangling part");
        }
        other => panic!("expected PathStat, got {other:?}"),
    }

    host.stop().expect("stop");
    drain_until_end!(host);
}

#[tokio::test]
async fn download_of_a_missing_source_is_failed_not_a_local_file() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("download-missing");
    let dest = dir.join("missing-download.bin");

    let report = host
        .download_file(
            DownloadRequest {
                source: RemotePath::Staging {
                    name: "does-not-exist.bin".to_string(),
                },
                dest: dest.clone(),
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("download runs");
    assert_eq!(report.outcome, JobOutcome::Failed);
    let error = report.error.expect("a failed download carries the reason");
    assert_eq!(error.code, ErrorCode::PathNotFound);
    assert_eq!(error.stage, "stat");
    assert!(!dest.exists(), "a failed download creates no local file");

    host.stop().expect("stop");
    drain_until_end!(host);
}

#[tokio::test]
async fn download_resumes_from_a_matching_temp() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("download-resume");
    let data = windowed_payload();
    let name = "download-resume.bin";
    let staged = || RemotePath::Staging {
        name: name.to_string(),
    };

    // Commit the artifact on the target by uploading it: a single whole-file
    // FsWrite cannot fit one secure-channel record, so let the engine chunk it.
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write the source");
    host.upload_file(
        UploadRequest {
            source: source.clone(),
            dest: staged(),
            source_label: "e2e-dlresume".to_string(),
            cleanup: CleanupPolicy::Keep,
            overwrite: false,
        },
        &CancelToken::new(),
        |_p| {},
    )
    .await
    .expect("stage the artifact via upload");

    // Pre-create a local temp holding a genuine prefix (an interrupted download).
    let dest = dir.join("download-resume.bin");
    let prefix = 300usize;
    std::fs::write(download_temp_path_for(&dest), &data[..prefix]).expect("seed the temp");

    let report = host
        .download_file(
            DownloadRequest {
                source: staged(),
                dest: dest.clone(),
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("download runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(
        report.resumed_from, prefix as u64,
        "the matching temp prefix is reused"
    );
    assert!(!report.restarted);
    assert_eq!(report.digest, sha256(&data));
    let got = std::fs::read(&dest).expect("read downloaded file");
    assert_eq!(
        got, data,
        "the resumed download reconstructs the exact source"
    );
    assert!(
        !download_temp_exists(&dest),
        "the verified rename consumes the temp"
    );

    host.stop().expect("stop");
    drain_until_end!(host);
}

/// The sibling temp path the download engine streams into, mirrored here so the
/// resume test can seed and inspect it.
fn download_temp_path_for(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".cretlink-part");
    match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// Whether a download temp still exists beside `dest`.
fn download_temp_exists(dest: &Path) -> bool {
    download_temp_path_for(dest).exists()
}

// --- an error on the technician's own file (ST-M003-005) --------------------
//
// A transfer reads or writes one file on the technician computer. An error
// there is that transfer's failure: it is reported in the stage `local` with
// the local path and the reason, it is never reported as a lost session, and
// the session stays usable. The connection's own errors are covered by
// `disconnect_mid_upload_is_honest_never_success` and still end the session.

/// Asserts `report` is a failed transfer caused by the technician's own file at
/// `local`, and returns its category and message for the checks that depend on
/// the cause.
fn assert_failed_locally(report: &TransferReport, local: &Path) -> (ErrorCode, String) {
    assert_eq!(report.outcome, JobOutcome::Failed, "{report:?}");
    assert!(
        report.digest.is_empty() && report.canonical_path.is_none(),
        "a failed transfer verifies and publishes nothing: {report:?}"
    );
    let error = report
        .error
        .as_ref()
        .expect("a failed transfer carries the reason");
    assert_eq!(error.stage, "local", "{error:?}");
    assert!(
        error.message.contains(&local.display().to_string()),
        "the message names the local path {}: {}",
        local.display(),
        error.message
    );
    assert!(
        error.message.contains("on the technician computer"),
        "the message says which computer the file is on: {}",
        error.message
    );
    (error.code, error.message.clone())
}

/// Runs a short command to its end: the session still carries work.
async fn assert_the_session_still_runs_a_command(host: &mut HostHandle) {
    assert!(host.control_active(), "the session must still be active");
    let job = host
        .run_command(ExecRequest::new(Shell::Cmd, "echo still-usable"))
        .await
        .expect("the session still accepts a command")
        .job;
    let result = collect_exec(host, job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("still-usable"),
        "stdout was {:?}",
        String::from_utf8_lossy(&result.stdout)
    );
}

/// Commits `data` to target staging under `name` through a real upload, from a
/// file in `dir`.
async fn stage_artifact(host: &HostHandle, dir: &Path, name: &str, data: &[u8]) {
    let source = dir.join(format!("{name}.source"));
    std::fs::write(&source, data).expect("write the source");
    let report = host
        .upload_file(
            UploadRequest {
                source,
                dest: RemotePath::Staging {
                    name: name.to_string(),
                },
                source_label: "e2e-local-file-error".to_string(),
                cleanup: CleanupPolicy::OnSessionEnd,
                overwrite: false,
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("stage the artifact via upload");
    assert_eq!(report.outcome, JobOutcome::Completed);
}

/// A download into a folder that does not exist on the technician computer is
/// a failed transfer that names the path and the reason, not a lost session:
/// the next command and the next transfer on the same session work.
#[tokio::test]
async fn download_into_a_missing_local_folder_is_failed_and_the_session_stays_usable() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("download-missing-local-folder");
    let data = pattern(4096);
    let name = "download-missing-local-folder.bin";
    let staged = || RemotePath::Staging {
        name: name.to_string(),
    };
    stage_artifact(&host, &dir, name, &data).await;

    // The destination's folder was never created.
    let folder = dir.join("no-such-folder");
    let dest = folder.join("download.bin");
    let report = host
        .download_file(
            DownloadRequest {
                source: staged(),
                dest: dest.clone(),
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("an error on the technician's own file is a report, not a lost session");
    let (code, message) = assert_failed_locally(&report, &dest);
    assert_eq!(code, ErrorCode::PathNotFound, "{message}");
    assert!(
        message.contains("the folder does not exist"),
        "the message gives the reason: {message}"
    );
    assert_eq!(report.sent, 0, "nothing was written");
    assert!(
        !folder.exists(),
        "a failed download creates neither the folder nor a file in it"
    );

    // The session is intact: a command runs, and the same download into a
    // folder that exists completes.
    assert_the_session_still_runs_a_command(&mut host).await;
    let dest = dir.join("download.bin");
    let report = host
        .download_file(
            DownloadRequest {
                source: staged(),
                dest: dest.clone(),
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("download runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(std::fs::read(&dest).expect("read downloaded file"), data);

    host.stop().expect("stop");
    drain_until_end!(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An upload of a file that does not exist on the technician computer is a
/// failed transfer that names the path and the reason, not a lost session:
/// nothing reaches the target, and the next command and the next transfer on
/// the same session work.
#[tokio::test]
async fn upload_of_a_missing_local_file_is_failed_and_the_session_stays_usable() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("upload-missing-local-file");
    let source = dir.join("no-such-file.bin");
    let name = "upload-missing-local-file.bin";
    let request = || UploadRequest {
        source: source.clone(),
        dest: RemotePath::Staging {
            name: name.to_string(),
        },
        source_label: "e2e-local-file-error".to_string(),
        cleanup: CleanupPolicy::OnSessionEnd,
        overwrite: false,
    };

    let report = host
        .upload_file(request(), &CancelToken::new(), |_p| {})
        .await
        .expect("an error on the technician's own file is a report, not a lost session");
    let (code, message) = assert_failed_locally(&report, &source);
    assert_eq!(code, ErrorCode::PathNotFound, "{message}");
    assert!(
        message.contains("the file does not exist"),
        "the message gives the reason: {message}"
    );
    assert_eq!(report.sent, 0, "nothing was sent");

    // Nothing reached the target: neither a final entry nor a pending part.
    match host
        .file_op(Operation::FsStat {
            path: RemotePath::Staging {
                name: name.to_string(),
            },
        })
        .await
        .expect("FsStat after the failed upload")
    {
        OpResult::PathStat { entry, partial_len } => {
            assert!(entry.is_none(), "no artifact was committed");
            assert!(partial_len.is_none(), "no part was staged");
        }
        other => panic!("expected PathStat, got {other:?}"),
    }

    // The session is intact: a command runs, and the same upload completes
    // once the file exists.
    assert_the_session_still_runs_a_command(&mut host).await;
    let data = pattern(4096);
    std::fs::write(&source, &data).expect("write the source");
    let report = host
        .upload_file(request(), &CancelToken::new(), |_p| {})
        .await
        .expect("upload runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(report.digest, sha256(&data));

    host.stop().expect("stop");
    drain_until_end!(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An upload whose source is a folder cannot be read: a failed transfer on the
/// technician's own file, with the operating system's reason, not a lost
/// session. (The category is the operating system's too, so it is not pinned:
/// reading a folder is refused on some systems and opening it on others.)
#[tokio::test]
async fn upload_of_a_local_folder_is_failed_and_the_session_stays_usable() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("upload-local-folder");
    let source = dir.join("a-folder");
    std::fs::create_dir(&source).expect("create the folder");
    let name = "upload-local-folder.bin";

    let report = host
        .upload_file(
            UploadRequest {
                source: source.clone(),
                dest: RemotePath::Staging {
                    name: name.to_string(),
                },
                source_label: "e2e-local-file-error".to_string(),
                cleanup: CleanupPolicy::OnSessionEnd,
                overwrite: false,
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("an error on the technician's own file is a report, not a lost session");
    assert_failed_locally(&report, &source);
    assert_eq!(report.sent, 0, "nothing was sent");

    // Nothing reached the target: neither a final entry nor a pending part.
    match host
        .file_op(Operation::FsStat {
            path: RemotePath::Staging {
                name: name.to_string(),
            },
        })
        .await
        .expect("FsStat after the failed upload")
    {
        OpResult::PathStat { entry, partial_len } => {
            assert!(entry.is_none(), "no artifact was committed");
            assert!(partial_len.is_none(), "no part was staged");
        }
        other => panic!("expected PathStat, got {other:?}"),
    }

    assert_the_session_still_runs_a_command(&mut host).await;

    host.stop().expect("stop");
    drain_until_end!(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A download whose destination cannot be replaced fails at its very end, when
/// every byte has arrived and been verified: still a failed transfer on the
/// technician's own file, not a lost session. Here the destination is a folder
/// that is not empty, which no system replaces with a file; the folder is left
/// as it was and the download's temp is removed.
#[tokio::test]
async fn download_onto_a_local_folder_is_failed_and_the_session_stays_usable() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("download-local-folder");
    let data = pattern(4096);
    let name = "download-local-folder.bin";
    stage_artifact(&host, &dir, name, &data).await;

    let dest = dir.join("a-folder");
    std::fs::create_dir(&dest).expect("create the folder");
    let kept = dest.join("kept.txt");
    std::fs::write(&kept, b"kept").expect("fill the folder");

    let report = host
        .download_file(
            DownloadRequest {
                source: RemotePath::Staging {
                    name: name.to_string(),
                },
                dest: dest.clone(),
            },
            &CancelToken::new(),
            |_p| {},
        )
        .await
        .expect("an error on the technician's own file is a report, not a lost session");
    assert_failed_locally(&report, &dest);
    assert_eq!(
        report.sent,
        data.len() as u64,
        "every byte had arrived before the destination refused the file"
    );
    assert_eq!(
        std::fs::read(&kept).expect("the folder keeps its content"),
        b"kept"
    );
    assert!(
        !download_temp_exists(&dest),
        "a failed download removes its temp"
    );

    assert_the_session_still_runs_a_command(&mut host).await;

    host.stop().expect("stop");
    drain_until_end!(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A download whose bytes cannot be written because the disk is full is a
/// failed transfer in the storage category: never a completed one with a short
/// file, and never a lost session. The engine writes in the background, so the
/// error of a write shows on the next write (the larger file) or only on the
/// final flush (the file of one chunk); both are covered.
///
/// Linux only: the test points the engine's sibling temp at `/dev/full`, the
/// device whose every write fails with "no space left", so no disk has to be
/// filled.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn download_onto_a_full_disk_is_failed_and_the_session_stays_usable() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("download-full-disk");
    for (name, data) in [
        ("download-full-disk-one-chunk.bin", pattern(4096)),
        ("download-full-disk-windowed.bin", windowed_payload()),
    ] {
        stage_artifact(&host, &dir, name, &data).await;
        let dest = dir.join(name);
        let temp = download_temp_path_for(&dest);
        std::os::unix::fs::symlink("/dev/full", &temp).expect("point the temp at a full device");

        let report = host
            .download_file(
                DownloadRequest {
                    source: RemotePath::Staging {
                        name: name.to_string(),
                    },
                    dest: dest.clone(),
                },
                &CancelToken::new(),
                |_p| {},
            )
            .await
            .expect("an error on the technician's own file is a report, not a lost session");
        let (code, message) = assert_failed_locally(&report, &dest);
        assert_eq!(code, ErrorCode::StorageExhausted, "{message}");
        assert!(
            message.contains("the disk is full"),
            "the message gives the reason: {message}"
        );
        assert!(!dest.exists(), "a failed download publishes no file");
        assert!(
            std::fs::symlink_metadata(&temp).is_err(),
            "a failed download removes its temp"
        );
    }

    assert_the_session_still_runs_a_command(&mut host).await;

    host.stop().expect("stop");
    drain_until_end!(host);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- revocation races against transfers and each other (ST-M003-007) -------
//
// The exec-side revoke-race is covered by
// `revoke_mid_job_reports_an_honest_outcome_never_success`; these add the two
// remaining races the story names: a revoke landing mid-transfer, and a
// simultaneous bilateral revoke. The single-sided customer/technician revokes
// and their fail-closed and non-re-joinable sub-claims are already covered by
// `console_revoke_tears_down_both_fail_closed` (which also proves a closed
// session is not re-joinable) and `agent_revoke_tears_down_both_fail_closed`.

/// A revoke that lands mid-upload is honest and leaves no residual: the transfer
/// never reports success and publishes no final file, and the agent still
/// autonomously sweeps its staging area at session end (REQ-S-004, REQ-N-007,
/// REQ-N-008, REQ-F-019).
///
/// Determinism without a sleep: [`large_payload`] needs many round trips through
/// the four-deep in-flight window (`MAX_INFLIGHT_FILE_OPS`), and the technician
/// revoke is fired from the first progress callback — the moment real bytes are
/// acknowledged in flight, long before the commit — so it always lands
/// mid-stream. `revoke` is a non-blocking send on `&self`, so firing it from the
/// progress callback (which shares `&host` with the running `upload_file`) is
/// sound.
#[tokio::test]
async fn revoke_mid_upload_is_honest_and_sweeps_staging() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("revoke-mid-upload");
    let data = large_payload();
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write source");
    let name = "revoke-mid-upload.bin";

    // Observe the in-process agent's real staging area: it roots its store under
    // `FileStore::for_session(session)`, so a second instance for the same id
    // yields the identical staging root (a pure mapping over the default base and
    // the session id). This lets the test check the interrupted upload part is
    // actually created mid-transfer and actually swept afterwards — not merely
    // that no removal error was reported.
    let session = host.session().expect("session id once active");
    let staging_root = FileStore::for_session(session).staging_root().to_path_buf();
    let part_path = staging_root.join(".parts").join(name);
    let final_path = staging_root.join(name);

    let cancel = CancelToken::new();
    let mut fired = false;
    let mut part_seen_mid = false;
    let report = host
        .upload_file(
            UploadRequest {
                source: source.clone(),
                dest: RemotePath::Staging {
                    name: name.to_string(),
                },
                source_label: "e2e-revoke".to_string(),
                cleanup: CleanupPolicy::OnSessionEnd,
                overwrite: false,
            },
            &cancel,
            |p: TransferProgress| {
                // Fire exactly once, the moment real bytes are in flight. A
                // non-zero `transferred` means at least one `FsWrite` was
                // acknowledged, so the agent has created and flushed the part.
                if !fired && p.transferred > 0 {
                    fired = true;
                    part_seen_mid = part_path.exists();
                    let _ = host.revoke(RevokeReason::TechnicianRequested);
                }
            },
        )
        .await;

    assert!(
        fired,
        "the revoke must be fired from a mid-stream progress callback"
    );
    assert!(
        part_seen_mid,
        "the interrupted upload must have created a staging part at {}",
        part_path.display()
    );

    // Honest outcome: a revoked transfer is never a success. It surfaces either
    // as a torn-down-session error, or as a non-Completed report that published
    // no final path. A commit is the ONLY route to a final file (ST-M003-004),
    // so "never Completed / no canonical path" is exactly "no torn final file".
    match report {
        Err(_) => {}
        Ok(r) => {
            assert_ne!(
                r.outcome,
                JobOutcome::Completed,
                "a revoked upload must never read as success"
            );
            assert!(
                r.canonical_path.is_none(),
                "a revoked upload must publish no final file"
            );
        }
    }

    // Both endpoints reach the terminal Revoked state and fail closed.
    assert_eq!(
        drain_until_revoked!(host),
        Some(RevokeReason::TechnicianRequested)
    );
    // Draining the agent to its close means its terminal path (which includes the
    // autonomous session-end staging sweep) has fully run.
    let _ = drain_until_revoked!(joiner);
    assert!(!host.control_active());
    assert!(!joiner.control_active());
    assert!(host.revoke(RevokeReason::TechnicianRequested).is_err());

    // The agent swept its staging area autonomously on the revoke path, so
    // staging never outlives the session (REQ-F-019, REQ-N-008): the orphaned
    // upload part observed mid-transfer is gone, and no committed final file was
    // ever published (the commit never ran) — a real "no torn file" check on the
    // agent's own filesystem, not just an absence-of-error signal.
    assert!(
        !part_path.exists(),
        "the session-end sweep must remove the orphaned upload part at {}",
        part_path.display()
    );
    assert!(
        !final_path.exists(),
        "a revoked upload must leave no committed final file at {}",
        final_path.display()
    );
    // The removal did not fail, so the honest teardown report stays clean.
    let agent_report = joiner
        .cleanup_report()
        .expect("the agent produces a cleanup report on the revoke path");
    assert!(
        agent_report.ok,
        "sweeping the part must not fail: {:?}",
        agent_report.notes
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A pure transport disconnect that lands **mid-upload** is honest and never
/// reads as success (REQ-N-008, REQ-S-004, REQ-S-009). This is the transfer-time
/// sibling of `disconnect_mid_session_is_honest_never_success`: where that drops
/// the agent during heartbeats, this drops it the moment real upload bytes are in
/// flight, so the console must still surface a terminal `Revoked { Error }` and
/// the interrupted upload must publish no final file — a hard disconnect is never
/// a completed transfer.
///
/// Determinism without a sleep: [`large_payload`] needs many round trips through
/// the four-deep in-flight window, and the agent handle is dropped from the first
/// progress callback — the moment a `FsWrite` is acknowledged — so the disconnect
/// always lands mid-stream, long before any commit. Dropping the [`JoinHandle`]
/// aborts the agent task, which closes the TCP socket with **no** Revoke frame: a
/// real hard disconnect, not a graceful revoke. The agent is therefore killed
/// before it can autonomously sweep, so — unlike the revoke mirror — the orphaned
/// part is expected to survive here; its recovery is the next agent start's job,
/// covered by `cleanup_failure_is_reported_then_retried_at_next_start` and the
/// `startup_sweep_*` suite.
#[tokio::test]
async fn disconnect_mid_upload_is_honest_never_success() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("disconnect-mid-upload");
    let data = large_payload();
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write source");
    let name = "disconnect-mid-upload.bin";

    // The in-process agent roots its store under `FileStore::for_session(session)`,
    // so this maps to the agent's real staging area and lets the test confirm the
    // interrupted part was actually created mid-transfer.
    let session = host.session().expect("session id once active");
    let staging_root = FileStore::for_session(session).staging_root().to_path_buf();
    let part_path = staging_root.join(".parts").join(name);
    let final_path = staging_root.join(name);

    // Move the agent handle into the callback so it can be dropped mid-stream.
    let mut agent = Some(joiner);
    let cancel = CancelToken::new();
    let mut fired = false;
    let mut part_seen_mid = false;
    let report = host
        .upload_file(
            UploadRequest {
                source: source.clone(),
                dest: RemotePath::Staging {
                    name: name.to_string(),
                },
                source_label: "e2e-disconnect".to_string(),
                cleanup: CleanupPolicy::OnSessionEnd,
                overwrite: false,
            },
            &cancel,
            |p: TransferProgress| {
                // Fire exactly once, the moment real bytes are in flight, then
                // hard-disconnect the agent by dropping its handle (aborts the
                // task → closes the socket with no Revoke frame).
                if !fired && p.transferred > 0 {
                    fired = true;
                    part_seen_mid = part_path.exists();
                    drop(agent.take());
                }
            },
        )
        .await;

    assert!(
        fired,
        "the disconnect must be triggered from a mid-stream progress callback"
    );
    assert!(
        part_seen_mid,
        "the interrupted upload must have created a staging part at {}",
        part_path.display()
    );

    // Honest outcome: a disconnected transfer is never a success. It surfaces
    // either as a torn-down-session error or as a non-Completed report that
    // published no final path. A commit is the ONLY route to a final file
    // (ST-M003-004), so "never Completed / no canonical path" is exactly "no torn
    // final file".
    match report {
        Err(_) => {}
        Ok(r) => {
            assert_ne!(
                r.outcome,
                JobOutcome::Completed,
                "a disconnected upload must never read as success"
            );
            assert!(
                r.canonical_path.is_none(),
                "a disconnected upload must publish no final file"
            );
        }
    }

    // The console surfaces an honest terminal outcome for the vanished peer: a
    // hard disconnect is `Revoked { Error }`, never a silent success.
    assert_eq!(
        drain_until_revoked!(host),
        Some(RevokeReason::Error),
        "a mid-upload disconnect must read as an honest Error, not success"
    );
    assert!(!host.control_active());
    assert!(host.revoke(RevokeReason::TechnicianRequested).is_err());

    // The commit never ran, so no final file was ever published — the honest
    // "no torn file" guarantee holds even though the agent had no chance to sweep.
    assert!(
        !final_path.exists(),
        "a disconnected upload must leave no committed final file at {}",
        final_path.display()
    );

    // The agent was killed before it could sweep, so the orphaned part may remain;
    // remove the whole session-scoped staging root here so this test leaves no
    // residue in the shared base (recovery-on-next-start is proven elsewhere).
    let _ = std::fs::remove_dir_all(&staging_root);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A simultaneous bilateral revoke — both endpoints revoke back to back before
/// either teardown is observed — terminates cleanly: each side reaches a
/// terminal Revoked state with no hang and no deadlock, and every post-teardown
/// action fails closed (REQ-S-004, REQ-S-009, REQ-N-004).
///
/// The reason each side reports may be its own or the racing peer's, so the
/// invariant under test is that both terminate and neither hangs — not which
/// reason wins. The `next_event!` timeout inside [`drain_until_revoked`] is what
/// turns a hypothetical deadlock into a test failure rather than a hang.
#[tokio::test]
async fn simultaneous_bilateral_revoke_terminates_both_without_hang() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    // Both sides revoke as close to simultaneously as one task allows, before
    // either teardown is drained, so the two teardowns genuinely race.
    host.revoke(RevokeReason::TechnicianRequested)
        .expect("console revoke");
    joiner
        .revoke(RevokeReason::CustomerRequested)
        .expect("agent revoke");

    // Neither side hangs: each drains to a terminal Revoked within the budget.
    assert!(
        drain_until_revoked!(host).is_some(),
        "the console must reach a terminal Revoked, not hang"
    );
    assert!(
        drain_until_revoked!(joiner).is_some(),
        "the agent must reach a terminal Revoked, not hang"
    );

    assert!(!host.control_active());
    assert!(!joiner.control_active());

    // Fail closed after a bilateral teardown: every further action is refused.
    assert!(host.revoke(RevokeReason::TechnicianRequested).is_err());
    assert!(host.stop().is_err());
    assert!(joiner.revoke(RevokeReason::CustomerRequested).is_err());
    assert!(joiner.grant_consent().is_err());

    // An honest cleanup report is available on both ends.
    assert!(host.cleanup_report().is_some_and(|r| r.ok));
    assert!(joiner.cleanup_report().is_some_and(|r| r.ok));
}

// --- reboot-resume arming reflection (ADR-0014, ST-M006-001) ---

/// The capability set a reboot-resume-capable pair advertises. Both endpoints
/// must advertise the identical set (the pairing prologue binds it), so the
/// console accepts the agent's armed-state reflection (REQ-N-012).
fn caps_with_reboot() -> Vec<String> {
    vec![
        "exec".to_string(),
        "files".to_string(),
        CAP_REBOOT_RESUME_V1.to_string(),
    ]
}

/// What a [`FakeArmer`] observed, so a test can prove the runtime handed it the
/// live session's Noise transcript hash and honest target (ADR-0014). It never
/// derives real key material — it only records and returns a fixed outcome.
#[derive(Debug, Default)]
struct FakeArmerState {
    armed: Vec<RebootResumeTicketId>,
    disarmed: Vec<RebootResumeTicketId>,
    last_hash: Option<[u8; 32]>,
    last_target: Option<TargetIdentity>,
    last_lifetime: Option<Duration>,
}

/// A test double for [`RebootResumeArmer`]: it records every arm/disarm and
/// returns a fixed ticket id and expiry so assertions are deterministic without
/// a clock or RNG in the runtime.
#[derive(Debug)]
struct FakeArmer {
    ticket_id: RebootResumeTicketId,
    not_after_unix_millis: i64,
    state: std::sync::Mutex<FakeArmerState>,
}

impl FakeArmer {
    fn new(ticket_id: RebootResumeTicketId, not_after_unix_millis: i64) -> Self {
        Self {
            ticket_id,
            not_after_unix_millis,
            state: std::sync::Mutex::new(FakeArmerState::default()),
        }
    }
}

impl RebootResumeArmer for FakeArmer {
    fn arm(
        &self,
        handshake_hash: &[u8; 32],
        target: &TargetIdentity,
        lifetime: Duration,
    ) -> Result<ArmOutcome, RebootResumeArmError> {
        let mut state = self.state.lock().unwrap();
        state.last_hash = Some(*handshake_hash);
        state.last_target = Some(target.clone());
        state.last_lifetime = Some(lifetime);
        state.armed.push(self.ticket_id);
        Ok(ArmOutcome {
            ticket_id: self.ticket_id,
            not_after_unix_millis: self.not_after_unix_millis,
        })
    }

    fn disarm(&self, ticket_id: &RebootResumeTicketId) -> bool {
        let mut state = self.state.lock().unwrap();
        state.disarmed.push(*ticket_id);
        state.armed.iter().any(|t| t == ticket_id)
    }
}

/// Binds a reboot-resume-capable console and dials it with an agent that carries
/// the given armer, returning both live handles before pairing completes.
async fn connected_pair_reboot(
    armer: Arc<dyn RebootResumeArmer>,
    audit: Arc<dyn SessionAudit>,
) -> (HostHandle, JoinHandle) {
    let host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps_with_reboot())
        .await
        .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join_with_reboot_resume(
        addr,
        agent_identity(),
        PROTOCOL_V1,
        caps_with_reboot(),
        audit,
        Arc::new(|| false),
        armer,
    )
    .await
    .expect("dial agent");
    (host, joiner)
}

/// A local arm gesture on a consented session mints a single-use ticket, hands
/// the armer the live transcript hash and honest target, reflects the clear
/// ticket metadata to **both** endpoints, and marks both "armed until" — while
/// granting no control (ADR-0014, ST-M006-001, REQ-S-001). A disarm reflects the
/// same way and clears both indicators.
#[tokio::test]
async fn reboot_resume_arm_and_disarm_reflect_to_both_endpoints() {
    const TICKET: RebootResumeTicketId = [7u8; 16];
    const NOT_AFTER: i64 = 1_800_000_000_000;
    let armer = Arc::new(FakeArmer::new(TICKET, NOT_AFTER));
    let audit = Arc::new(RecordingAudit::default());
    let (mut host, mut joiner) =
        connected_pair_reboot(armer.clone(), audit.clone() as Arc<dyn SessionAudit>).await;
    drive_to_active(&mut host, &mut joiner).await;

    // Arming never restores or grants control on its own.
    joiner
        .arm_reboot_resume(Duration::from_secs(600))
        .expect("arm");

    match next_event!(joiner) {
        SessionEvent::RebootResumeArmed {
            ticket_id,
            not_after_unix_millis,
        } => {
            assert_eq!(ticket_id, TICKET);
            assert_eq!(not_after_unix_millis, NOT_AFTER);
        }
        other => panic!("expected the agent's own RebootResumeArmed, got {other:?}"),
    }
    match next_event!(host) {
        SessionEvent::RebootResumeArmed {
            ticket_id,
            not_after_unix_millis,
        } => {
            assert_eq!(ticket_id, TICKET);
            assert_eq!(not_after_unix_millis, NOT_AFTER);
        }
        other => panic!("expected the console's RebootResumeArmed reflection, got {other:?}"),
    }

    // Both display indicators show the same expiry; control is still active but
    // was never granted *by* arming.
    wait_until(|| joiner.reboot_resume_armed_until() == Some(NOT_AFTER)).await;
    wait_until(|| host.reboot_resume_armed_until() == Some(NOT_AFTER)).await;
    assert!(joiner.control_active());
    assert!(host.control_active());

    // The armer received the live session's transcript hash and honest target.
    {
        let state = armer.state.lock().unwrap();
        assert_eq!(state.last_target.as_ref(), Some(&agent_identity()));
        assert_eq!(state.last_lifetime, Some(Duration::from_secs(600)));
        assert!(
            state.last_hash.is_some(),
            "the armer must be handed the Noise transcript hash"
        );
        assert_eq!(state.armed, vec![TICKET]);
    }

    // Disarm reflects to both and clears both indicators.
    joiner.disarm_reboot_resume(TICKET).expect("disarm");
    match next_event!(joiner) {
        SessionEvent::RebootResumeDisarmed { ticket_id } => assert_eq!(ticket_id, TICKET),
        other => panic!("expected the agent's own RebootResumeDisarmed, got {other:?}"),
    }
    match next_event!(host) {
        SessionEvent::RebootResumeDisarmed { ticket_id } => assert_eq!(ticket_id, TICKET),
        other => panic!("expected the console's RebootResumeDisarmed reflection, got {other:?}"),
    }
    wait_until(|| joiner.reboot_resume_armed_until().is_none()).await;
    wait_until(|| host.reboot_resume_armed_until().is_none()).await;

    // The agent audited the honest arm and disarm.
    let actions: Vec<&'static str> = {
        let records = audit.records.lock().unwrap();
        records.iter().map(|(_, action)| *action).collect()
    };
    assert!(actions.contains(&"reboot_resume_armed"));
    assert!(actions.contains(&"reboot_resume_disarmed"));
}

/// An agent left at the default (off-by-default) reboot-resume refuses every arm
/// request: nothing is reflected to the console, no indicator is set, the refusal
/// is recorded honestly, and the session stays healthy (ADR-0014, REQ-S-001).
#[tokio::test]
async fn reboot_resume_off_by_default_refuses_arming() {
    // Both advertise the capability (so pairing matches), but the agent installs
    // no armer — `join_with_audit` uses the default `NoRebootResume`.
    let audit = Arc::new(RecordingAudit::default());
    let host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps_with_reboot())
        .await
        .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join_with_audit(
        addr,
        agent_identity(),
        PROTOCOL_V1,
        caps_with_reboot(),
        audit.clone(),
    )
    .await
    .expect("dial agent");
    let (mut host, mut joiner) = (host, joiner);
    drive_to_active(&mut host, &mut joiner).await;

    joiner
        .arm_reboot_resume(Duration::from_secs(600))
        .expect("arm request is accepted by the channel");

    // The refusal is recorded honestly on the agent...
    wait_until(|| {
        audit
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|(_, action)| *action == "reboot_resume_arm_refused")
    })
    .await;

    // ...nothing is reflected to the console (no armed event within a window)...
    assert!(
        timeout(Duration::from_millis(300), host.next_event())
            .await
            .is_err(),
        "a refused arm must never reflect to the console"
    );

    // ...no indicator is set on either side, and control is untouched.
    assert_eq!(joiner.reboot_resume_armed_until(), None);
    assert_eq!(host.reboot_resume_armed_until(), None);
    assert!(joiner.control_active());
    assert!(host.control_active());
}

/// A reboot-resume arm gesture made **before** the session is consented and
/// active is never honored: it is not replayed the instant consent lands, so the
/// armer stays untouched and neither endpoint shows an armed indicator. A fresh
/// gesture made once active still works, proving only the stale pre-consent
/// command is discarded (ADR-0014, ST-M006-001 — "arming refused with no
/// attended grant"; REQ-S-001).
#[tokio::test]
async fn reboot_resume_arm_before_consent_is_ignored() {
    const TICKET: RebootResumeTicketId = [9u8; 16];
    const NOT_AFTER: i64 = 1_800_000_000_000;
    let armer = Arc::new(FakeArmer::new(TICKET, NOT_AFTER));
    let audit = Arc::new(RecordingAudit::default());
    let (mut host, mut joiner) =
        connected_pair_reboot(armer.clone(), audit.clone() as Arc<dyn SessionAudit>).await;

    // Queue an arm gesture while the session is still pre-consent.
    joiner
        .arm_reboot_resume(Duration::from_secs(600))
        .expect("arm request is accepted by the channel");

    drive_to_active(&mut host, &mut joiner).await;
    wait_until(|| joiner.control_active()).await;

    // The stale pre-consent command armed nothing: the armer was never invoked,
    // and neither endpoint shows an armed indicator.
    assert_eq!(
        armer.state.lock().unwrap().armed.len(),
        0,
        "a pre-consent arm gesture must not arm once consent lands"
    );
    assert_eq!(joiner.reboot_resume_armed_until(), None);
    assert_eq!(host.reboot_resume_armed_until(), None);

    // A gesture made now — inside the consented, active session — still arms.
    joiner
        .arm_reboot_resume(Duration::from_secs(600))
        .expect("arm");
    match next_event!(joiner) {
        SessionEvent::RebootResumeArmed { ticket_id, .. } => assert_eq!(ticket_id, TICKET),
        other => panic!("expected RebootResumeArmed after a live gesture, got {other:?}"),
    }
    wait_until(|| joiner.reboot_resume_armed_until() == Some(NOT_AFTER)).await;
}

// --- Stated purpose and customer-visible actions (REQ-F-026, ST-M009-007) ---

/// Reads agent events until the next `RequestHandled`, skipping job events.
async fn next_handled(joiner: &mut JoinHandle) -> SessionEvent {
    loop {
        let event = next_event!(joiner);
        if matches!(event, SessionEvent::RequestHandled { .. }) {
            return event;
        }
    }
}

/// The parts of a `RequestHandled` a test asserts: everything but the minted
/// id and the measured duration.
fn handled_parts(
    event: SessionEvent,
) -> (
    cretlink_session::Origin,
    Option<cretlink_session::Purpose>,
    cretlink_session::ActivityKind,
    String,
    Option<String>,
    Option<u64>,
    cretlink_session::ActionOutcome,
) {
    match event {
        SessionEvent::RequestHandled {
            origin,
            purpose,
            kind,
            operation,
            path,
            bytes,
            outcome,
            ..
        } => (origin, purpose, kind, operation, path, bytes, outcome),
        other => panic!("expected RequestHandled, got {other:?}"),
    }
}

/// The agent reports every handled request with who drove it, the caller's
/// stated purpose (AI only), the real operation, and the honest result; a
/// started command carries its purpose on `CommandStarted` instead, and a
/// technician request carries no purpose.
#[tokio::test]
async fn agent_reports_each_action_with_source_purpose_operation_and_result() {
    use cretlink_session::{ActionOutcome, ActivityKind, Attribution, Origin, Purpose};

    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;
    let ai = |text: &str| Attribution::new(Origin::Mcp, Some(Purpose::new(text).unwrap()));

    // An AI request that succeeds.
    host.system_info_as(ai("Checking which computer this is"))
        .await
        .expect("system info");
    let first = next_handled(&mut joiner).await;
    let first_id = match &first {
        SessionEvent::RequestHandled {
            id, duration_ms, ..
        } => {
            assert!(duration_ms.is_some(), "a handled request has a duration");
            *id
        }
        _ => unreachable!(),
    };
    assert_eq!(
        handled_parts(first),
        (
            Origin::Mcp,
            Some(Purpose::new("Checking which computer this is").unwrap()),
            ActivityKind::SystemInformation,
            "read system information".to_string(),
            None,
            None,
            ActionOutcome::Succeeded,
        )
    );

    // A technician request: labelled by its source, no purpose, a fresh id.
    host.list_jobs().await.expect("list jobs");
    let second = next_handled(&mut joiner).await;
    assert!(matches!(second, SessionEvent::RequestHandled { id, .. } if id > first_id));
    assert_eq!(
        handled_parts(second),
        (
            Origin::Console,
            None,
            ActivityKind::JobControl,
            "list running jobs".to_string(),
            None,
            None,
            ActionOutcome::Succeeded,
        )
    );

    // An AI cancellation the agent refuses: the purpose rides the wire
    // `Cancel`, and the honest failure is reported, never a success.
    let refused = host
        .cancel_job_as(JobId(999), ai("Stopping the stuck installer"))
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        handled_parts(next_handled(&mut joiner).await),
        (
            Origin::Mcp,
            Some(Purpose::new("Stopping the stuck installer").unwrap()),
            ActivityKind::JobControl,
            "stop job 999".to_string(),
            None,
            None,
            ActionOutcome::Failed {
                code: ErrorCode::UnknownJob
            },
        )
    );

    // A started command carries its purpose on `CommandStarted`, beside the
    // real command, and is not reported a second time as a handled request.
    let job = host
        .run_command_as(
            ExecRequest::new(Shell::Cmd, "echo purpose"),
            ai("Checking that commands run"),
        )
        .await
        .expect("run")
        .job;
    loop {
        match next_event!(joiner) {
            SessionEvent::CommandStarted {
                command,
                origin,
                purpose,
                ..
            } => {
                assert_eq!(command, "echo purpose");
                assert_eq!(origin, Origin::Mcp);
                assert_eq!(
                    purpose.as_ref().map(Purpose::as_str),
                    Some("Checking that commands run")
                );
                break;
            }
            SessionEvent::RequestHandled { operation, .. } => {
                panic!("a started command must not be reported twice: {operation}")
            }
            _ => {}
        }
    }
    let _ = collect_exec(&mut host, job).await;
    host.system_info().await.expect("system info");
    match next_handled(&mut joiner).await {
        SessionEvent::RequestHandled { operation, .. } => {
            assert_eq!(operation, "read system information");
        }
        other => panic!("unexpected {other:?}"),
    }

    host.stop().expect("stop");
}

// --- One activity item for both surfaces (ST-M009-011) ---

/// The console's display name rides the pairing handshake (protocol minor 10):
/// the agent knows what the technician's computer says once connected.
#[tokio::test]
async fn the_agent_learns_the_technician_display_name_from_the_handshake() {
    let (mut host, mut joiner) = connected_pair().await;
    assert_eq!(next_event!(joiner), SessionEvent::Connected);
    assert_eq!(
        joiner
            .technician_display_name()
            .as_ref()
            .map(cretlink_protocol::DisplayName::as_str),
        Some("Test Technician")
    );
    host.stop().expect("stop");
    drain_until_end!(host);
}

/// A file reaching the customer computer is reported at its commit with its
/// byte count; a file leaving it is reported when sending starts and again,
/// under the same id, when it completes, with the byte count — and the
/// customer-side fold turns that into one "file sent" item updated in place.
#[tokio::test]
async fn files_sent_and_received_are_reported_with_their_byte_count() {
    use cretlink_session::{ActionOutcome, ActivityKind, ActivityState, CustomerActivity};

    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("activity-bytes");
    let data = windowed_payload();
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write source");
    let staged = || RemotePath::Staging {
        name: "activity.bin".to_string(),
    };
    let cancel = CancelToken::new();
    let report = host
        .upload_file(
            UploadRequest {
                source: source.clone(),
                dest: staged(),
                source_label: "e2e-activity".to_string(),
                cleanup: CleanupPolicy::Keep,
                overwrite: false,
            },
            &cancel,
            |_| {},
        )
        .await
        .expect("upload runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    let report = host
        .download_file(
            DownloadRequest {
                source: staged(),
                dest: dir.join("downloaded.bin"),
            },
            &cancel,
            |_| {},
        )
        .await
        .expect("download runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    // One more request after the transfer, so the stream has a known end.
    host.list_jobs().await.expect("list jobs");

    let total = data.len() as u64;
    let mut fold = CustomerActivity::new();
    let mut received = None;
    let mut sent = Vec::new();
    loop {
        let event = next_handled(&mut joiner).await;
        fold.apply(&event, 0);
        let SessionEvent::RequestHandled {
            id,
            kind,
            bytes,
            outcome,
            operation,
            ..
        } = event
        else {
            unreachable!()
        };
        match kind {
            ActivityKind::FileReceived => received = Some((bytes, outcome)),
            ActivityKind::FileSent => sent.push((id, bytes, outcome)),
            ActivityKind::JobControl if operation == "list running jobs" => break,
            _ => {}
        }
    }
    assert_eq!(received, Some((Some(total), ActionOutcome::Succeeded)));
    assert_eq!(sent.len(), 2, "{sent:?}");
    let (start_id, start_bytes, start_outcome) = sent[0];
    let (end_id, end_bytes, end_outcome) = sent[1];
    assert_eq!(start_id, end_id, "start and end share one item id");
    assert_eq!(
        (start_bytes, start_outcome),
        (Some(total), ActionOutcome::InProgress)
    );
    assert_eq!(
        (end_bytes, end_outcome),
        (Some(total), ActionOutcome::Succeeded)
    );

    let file_sent: Vec<_> = fold
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            cretlink_session::ActivityEntry::Item { item }
                if item.kind == ActivityKind::FileSent =>
            {
                Some(item)
            }
            _ => None,
        })
        .collect();
    assert_eq!(file_sent.len(), 1, "updated in place: {file_sent:?}");
    assert_eq!(file_sent[0].bytes, Some(total));
    assert_eq!(file_sent[0].state, ActivityState::Done { exit_code: None });

    host.stop().expect("stop");
    drain_until_end!(host);
}

/// Every request the agent refuses reaches the customer feed as a failed item
/// with the reason in plain words and no file content (ST-M009-011, REQ-F-017):
/// a request bound to another session, and a refused transfer step — once per
/// path until that path's next reported request, so the chunks of one refused
/// transfer do not flood the feed.
#[tokio::test]
async fn requests_the_agent_refuses_reach_the_customer_feed() {
    use cretlink_session::{ActionOutcome, ActivityKind, ActivityState, CustomerActivity};

    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;
    let mut fold = CustomerActivity::new();

    // A download read bound to another session: refused, never run.
    let source = RemotePath::Staging {
        name: "refused.bin".to_string(),
    };
    host.file_op_with_session(
        SessionId::new(),
        Operation::FsRead {
            path: source.clone(),
            offset: 0,
            len: 16,
        },
    )
    .await
    .expect_err("a wrong-session read is refused");
    let event = next_handled(&mut joiner).await;
    fold.apply(&event, 0);
    let (_, _, kind, _, path, bytes, outcome) = handled_parts(event);
    assert_eq!(kind, ActivityKind::FileSent);
    assert_eq!(
        path.as_deref(),
        Some("refused.bin (CretLink staging folder)")
    );
    assert_eq!(bytes, None);
    assert_eq!(
        outcome,
        ActionOutcome::Failed {
            code: ErrorCode::UnknownSession
        }
    );

    // A refused chunk write (an offset the part does not have), twice: one item.
    let dest = RemotePath::Staging {
        name: "refused-upload.bin".to_string(),
    };
    let write = |offset| Operation::FsWrite {
        dest: dest.clone(),
        offset,
        data: B64Bytes::new(vec![0xAB; 8]),
    };
    for _ in 0..2 {
        host.file_op(write(64))
            .await
            .expect_err("a write at a wrong offset is refused");
    }
    // A reported request on the same path, then another refused write: a new
    // item, so a later refused transfer is not hidden by an earlier one.
    host.file_op(Operation::FsStat { path: dest.clone() })
        .await
        .expect("stat");
    host.file_op(write(128)).await.expect_err("refused again");
    host.list_jobs().await.expect("list jobs");

    let mut refused_writes = 0;
    loop {
        let event = next_handled(&mut joiner).await;
        fold.apply(&event, 0);
        let (_, _, kind, operation, _, bytes, outcome) = handled_parts(event);
        if operation.starts_with("write ") {
            refused_writes += 1;
            assert_eq!(kind, ActivityKind::FileOperation);
            assert_eq!(bytes, None, "no byte count for a refused chunk");
            assert_eq!(
                outcome,
                ActionOutcome::Failed {
                    code: ErrorCode::WriteConflict
                }
            );
        } else if operation == "list running jobs" {
            break;
        }
    }
    assert_eq!(refused_writes, 2, "one per refused transfer, not per chunk");

    // The customer's fold shows them failed, with the reason in words.
    let failed: Vec<_> = fold
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            cretlink_session::ActivityEntry::Item { item } => match item.state {
                ActivityState::Failed { message, .. } => Some(message),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(failed.len(), 3, "{failed:?}");
    assert_eq!(
        failed[0].as_deref(),
        Some("the request was not for this session")
    );
    assert_eq!(
        failed[1].as_deref(),
        Some("the file changed during the transfer")
    );

    host.stop().expect("stop");
    drain_until_end!(host);
}

/// When the agent's event channel is under pressure, activity events are shed
/// to keep the lifecycle reserve free — and counted: the next activity event is
/// preceded by one `ActivityShed` marker, so handled plus shed events add up to
/// every request (ST-M009-011, REQ-N-008).
#[tokio::test]
async fn shed_activity_events_are_counted_and_reported_where_they_were_dropped() {
    let (mut host, mut joiner) = connected_pair().await;
    drive_to_active(&mut host, &mut joiner).await;

    // Far more requests than the agent's bounded channel holds, without reading
    // the agent's events, so most of them are shed.
    const BURST: u64 = 200;
    for _ in 0..BURST {
        host.list_jobs().await.expect("list jobs");
    }
    let mut handled = 0u64;
    let mut shed = 0u64;
    // Drain what was buffered, then one more request delivers the marker.
    while let Ok(Some(event)) = timeout(Duration::from_millis(300), joiner.next_event()).await {
        match event {
            SessionEvent::RequestHandled { .. } => handled += 1,
            SessionEvent::ActivityShed { count } => shed += count,
            _ => {}
        }
    }
    assert!(handled < BURST, "the burst must overflow the channel");
    host.list_jobs().await.expect("list jobs");
    match next_event!(joiner) {
        SessionEvent::ActivityShed { count } => shed += count,
        other => panic!("expected the shed marker first, got {other:?}"),
    }
    assert!(matches!(
        next_event!(joiner),
        SessionEvent::RequestHandled { .. }
    ));
    handled += 1;
    assert_eq!(
        handled + shed,
        BURST + 1,
        "every request is either shown or counted"
    );

    host.stop().expect("stop");
    drain_until_end!(host);
}

// --- The file operations an explorer needs (ST-M010-001) ---------------------
//
// The drive list and the removal of a folder with its content run over the
// real session against the in-process agent. A folder to remove is planted in
// that agent's own staging area and named by its staging name: the wire takes
// absolute paths in the Windows form only, so a staging name is the one way to
// name a real folder on every platform these tests run on. Sending a file to
// a folder by an absolute path is covered where such a path exists, on
// Windows.

/// The default capabilities plus the explorer file capability, so the agent
/// serves the drive list and the folder removal instead of answering
/// `Unsupported` (REQ-N-012).
fn caps_with_explorer() -> Vec<String> {
    let mut capabilities = caps();
    capabilities.push(cretlink_session::CAP_FS_V2.to_string());
    capabilities
}

/// An audit sink that keeps every record the agent's runtime writes, with its
/// outcome.
#[derive(Debug, Default)]
struct OutcomeAudit {
    records: std::sync::Mutex<Vec<(Actor, &'static str, Outcome)>>,
}

impl OutcomeAudit {
    /// The outcomes recorded for `action`, in order.
    fn of(&self, action: &str) -> Vec<Outcome> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, recorded, _)| *recorded == action)
            .map(|(_, _, outcome)| *outcome)
            .collect()
    }
}

impl SessionAudit for OutcomeAudit {
    fn record(&self, actor: Actor, action: &'static str, outcome: Outcome) {
        self.records.lock().unwrap().push((actor, action, outcome));
    }
}

/// A connected console/agent pair whose agent advertises the explorer file
/// capability and records its audit stream into `audit`.
async fn connected_pair_explorer(audit: Arc<OutcomeAudit>) -> (HostHandle, JoinHandle) {
    let host = SessionHost::host(
        loopback(),
        console_hints(),
        PROTOCOL_V1,
        caps_with_explorer(),
    )
    .await
    .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let joiner = SessionJoiner::join_with_audit(
        addr,
        agent_identity(),
        PROTOCOL_V1,
        caps_with_explorer(),
        audit,
    )
    .await
    .expect("dial agent");
    (host, joiner)
}

/// Plants the folder `name` in the staging area of the session's in-process
/// agent and returns its real path. The agent roots its store under
/// `FileStore::for_session(session)`, so a second instance for the same id
/// yields the identical staging root.
fn staged_folder(host: &HostHandle, name: &str) -> PathBuf {
    let session = host.session().expect("session id once active");
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

/// The words the agent describes the removal of the staged folder `name` in.
fn removal_of(name: &str) -> String {
    format!("delete the folder {name} (CretLink staging folder) and everything in it")
}

/// The drives are listed over the session, and the customer sees that the
/// technician asked for them, as a file operation without a path.
#[tokio::test]
async fn the_drives_are_listed_over_the_session() {
    use cretlink_session::{ActionOutcome, ActivityKind, Origin};

    let (mut host, mut joiner) = connected_pair_explorer(Arc::default()).await;
    drive_to_active(&mut host, &mut joiner).await;

    let drives = host.list_drives().await.expect("the drives are listed");
    assert!(!drives.is_empty(), "a computer has at least one drive");
    for drive in &drives {
        assert!(!drive.root.is_empty());
        if let (Some(total), Some(free)) = (drive.total_bytes, drive.free_bytes) {
            assert!(free <= total, "{drive:?}");
        }
    }
    // Off Windows the one drive is the root file system.
    #[cfg(unix)]
    assert_eq!(
        drives.iter().map(|d| d.root.as_str()).collect::<Vec<_>>(),
        ["/"]
    );

    assert_eq!(
        handled_parts(next_handled(&mut joiner).await),
        (
            Origin::Console,
            None,
            ActivityKind::FileOperation,
            "list the drives".to_string(),
            None,
            None,
            ActionOutcome::Succeeded,
        )
    );

    host.stop().expect("stop");
    drain_until_end!(host);
}

/// An agent that does not advertise the explorer file capability answers the
/// drive list and the folder removal with `Unsupported` before anything is
/// listed or removed. The customer still sees the refused requests, nothing is
/// recorded as a removal, and the session stays usable (REQ-N-012).
#[tokio::test]
async fn without_the_capability_the_explorer_operations_are_unsupported() {
    use cretlink_session::{ActionOutcome, ActivityKind};

    let audit = Arc::new(OutcomeAudit::default());
    let host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps())
        .await
        .expect("bind console");
    let addr = host.local_addr().expect("listener address");
    // caps() omits fs.v2.
    let joiner =
        SessionJoiner::join_with_audit(addr, agent_identity(), PROTOCOL_V1, caps(), audit.clone())
            .await
            .expect("dial agent");
    let (mut host, mut joiner) = (host, joiner);
    drive_to_active(&mut host, &mut joiner).await;
    let folder = staged_folder(&host, "kept");
    plant_files(&folder, 3);

    match host.list_drives().await {
        Err(SessionError::Remote { code, .. }) => assert_eq!(code, ErrorCode::Unsupported),
        other => panic!("expected an Unsupported remote error, got {other:?}"),
    }
    let (_, _, kind, operation, _, _, outcome) = handled_parts(next_handled(&mut joiner).await);
    assert_eq!(kind, ActivityKind::FileOperation);
    assert_eq!(operation, "list the drives");
    assert_eq!(
        outcome,
        ActionOutcome::Failed {
            code: ErrorCode::Unsupported
        }
    );

    // The removal is a failed report that names the refusal, never an error
    // of the session and never a success.
    let report = host
        .remove_tree(staged("kept"), &CancelToken::new(), |_| {})
        .await
        .expect("a refused removal is a report");
    assert_eq!(report.outcome, JobOutcome::Failed);
    assert_eq!(
        report.error.as_ref().map(|error| error.code),
        Some(ErrorCode::Unsupported)
    );
    assert_eq!((report.removed_files, report.removed_dirs), (0, 0));
    assert_eq!(entries_in(&folder), 3, "nothing was removed");
    let (_, _, _, operation, path, _, outcome) = handled_parts(next_handled(&mut joiner).await);
    assert_eq!(operation, removal_of("kept"));
    assert_eq!(path.as_deref(), Some("kept (CretLink staging folder)"));
    assert_eq!(
        outcome,
        ActionOutcome::Failed {
            code: ErrorCode::Unsupported
        }
    );

    assert_the_session_still_runs_a_command(&mut host).await;
    host.stop().expect("stop");
    drain_until_end!(host);
    drain_until_end!(joiner);
    assert!(
        audit.of("fs_remove_tree").is_empty() && audit.of("fs_remove_tree_start").is_empty(),
        "a request refused at the capability gate is not a removal"
    );
    let _ = std::fs::remove_dir_all(&folder);
}

/// One call removes a folder with everything in it. The customer sees it as
/// one action with its kind and path, and the agent's audit trail records it.
#[tokio::test]
async fn a_folder_is_removed_with_its_content_over_the_session() {
    use cretlink_session::{ActionOutcome, ActivityKind, Origin, TreeRemovalProgress};

    let audit = Arc::new(OutcomeAudit::default());
    let (mut host, mut joiner) = connected_pair_explorer(audit.clone()).await;
    drive_to_active(&mut host, &mut joiner).await;
    let folder = staged_folder(&host, "old-setup");
    plant_files(&folder, 5);
    std::fs::create_dir_all(folder.join("sub").join("deeper")).expect("plant");
    plant_files(&folder.join("sub").join("deeper"), 2);

    let mut progress: Vec<TreeRemovalProgress> = Vec::new();
    let report = host
        .remove_tree(staged("old-setup"), &CancelToken::new(), |p| {
            progress.push(p)
        })
        .await
        .expect("the removal runs");

    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(report.removed_files, 7);
    assert_eq!(report.removed_dirs, 3, "deeper, sub and the folder itself");
    assert!(report.failed.is_empty() && report.failed_more == 0);
    assert!(report.error.is_none() && !report.blocked);
    assert!(!folder.exists(), "the folder is gone");
    assert_eq!(
        progress.last(),
        Some(&TreeRemovalProgress {
            removed_files: 7,
            removed_dirs: 3
        })
    );

    assert_eq!(
        handled_parts(next_handled(&mut joiner).await),
        (
            Origin::Console,
            None,
            ActivityKind::FileOperation,
            removal_of("old-setup"),
            Some("old-setup (CretLink staging folder)".to_string()),
            None,
            ActionOutcome::Succeeded,
        )
    );

    host.stop().expect("stop");
    drain_until_end!(host);
    drain_until_end!(joiner);
    assert_eq!(audit.of("fs_remove_tree"), vec![Outcome::Ok]);
}

/// A folder with more entries than one step settles is removed by several
/// requests, which the engine issues on its own. The customer sees one action:
/// in progress from the first step, done with the last, under one id.
#[tokio::test]
async fn a_large_folder_is_removed_in_steps_as_one_action() {
    use cretlink_protocol::limits::MAX_TREE_STEP_ENTRIES;
    use cretlink_session::{ActionOutcome, Attribution, Origin, Purpose};

    let audit = Arc::new(OutcomeAudit::default());
    let (mut host, mut joiner) = connected_pair_explorer(audit.clone()).await;
    drive_to_active(&mut host, &mut joiner).await;
    let folder = staged_folder(&host, "node_modules");
    let files = MAX_TREE_STEP_ENTRIES + 300;
    plant_files(&folder, files);

    // Attributed to its origin with the caller's stated purpose, like any
    // other request (REQ-F-017, REQ-F-026).
    let purpose = Purpose::new("Removing the broken package folder").unwrap();
    let mut steps = 0;
    let report = host
        .remove_tree_as(
            staged("node_modules"),
            Attribution::new(Origin::Mcp, Some(purpose.clone())),
            &CancelToken::new(),
            |_| steps += 1,
        )
        .await
        .expect("the removal runs");

    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!(report.removed_files, files);
    assert_eq!(report.removed_dirs, 1);
    assert!(steps >= 2, "more entries than one step settles: {steps}");
    assert!(!folder.exists());

    let (first_id, first) = match next_handled(&mut joiner).await {
        SessionEvent::RequestHandled {
            id,
            origin,
            purpose: stated,
            outcome,
            duration_ms,
            ..
        } => {
            assert_eq!(origin, Origin::Mcp);
            assert_eq!(stated, Some(purpose.clone()));
            assert_eq!(duration_ms, None, "a removal in progress has no duration");
            (id, outcome)
        }
        other => panic!("expected RequestHandled, got {other:?}"),
    };
    assert_eq!(first, ActionOutcome::InProgress);
    match next_handled(&mut joiner).await {
        SessionEvent::RequestHandled {
            id,
            origin,
            purpose: stated,
            operation,
            outcome,
            duration_ms,
            ..
        } => {
            assert_eq!(id, first_id, "the same action, now ended");
            assert_eq!(origin, Origin::Mcp);
            assert_eq!(stated, Some(purpose));
            assert_eq!(operation, removal_of("node_modules"));
            assert_eq!(outcome, ActionOutcome::Succeeded);
            assert!(duration_ms.is_some());
        }
        other => panic!("expected RequestHandled, got {other:?}"),
    }

    host.stop().expect("stop");
    drain_until_end!(host);
    drain_until_end!(joiner);
    assert_eq!(audit.of("fs_remove_tree_start"), vec![Outcome::Ok]);
    assert_eq!(audit.of("fs_remove_tree"), vec![Outcome::Ok]);
}

/// A removal that is cancelled ends between two steps: nothing more is
/// removed, the report says what was removed until then and is never a
/// success, and the customer sees the action end as cancelled rather than
/// keep running (REQ-N-008).
#[tokio::test]
async fn a_cancelled_removal_stops_between_steps_and_says_so_on_both_sides() {
    use cretlink_protocol::limits::MAX_TREE_STEP_ENTRIES;
    use cretlink_session::ActionOutcome;

    let audit = Arc::new(OutcomeAudit::default());
    let (mut host, mut joiner) = connected_pair_explorer(audit.clone()).await;
    drive_to_active(&mut host, &mut joiner).await;
    let folder = staged_folder(&host, "half");
    let files = MAX_TREE_STEP_ENTRIES + 300;
    plant_files(&folder, files);

    // Cancel as soon as the first step has been answered.
    let cancel = CancelToken::new();
    let report = host
        .remove_tree(staged("half"), &cancel, |_| cancel.cancel())
        .await
        .expect("a cancelled removal is a report");

    assert_eq!(report.outcome, JobOutcome::Cancelled, "never a success");
    assert!(report.removed_files > 0, "the first step removed something");
    assert!(
        report.removed_files < files,
        "and the cancel stopped the rest"
    );
    assert_eq!(report.removed_dirs, 0);
    assert_eq!(
        entries_in(&folder),
        files - report.removed_files,
        "what was not removed is still there"
    );

    let first_id = match next_handled(&mut joiner).await {
        SessionEvent::RequestHandled { id, outcome, .. } => {
            assert_eq!(outcome, ActionOutcome::InProgress);
            id
        }
        other => panic!("expected RequestHandled, got {other:?}"),
    };
    match next_handled(&mut joiner).await {
        SessionEvent::RequestHandled {
            id,
            operation,
            outcome,
            ..
        } => {
            assert_eq!(id, first_id, "the same action, now ended");
            assert_eq!(operation, removal_of("half"));
            assert_eq!(outcome, ActionOutcome::Cancelled);
        }
        other => panic!("expected RequestHandled, got {other:?}"),
    }

    // A removal cancelled before it began sends nothing at all.
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let report = host
        .remove_tree(staged("half"), &cancelled, |_| {})
        .await
        .expect("a report");
    assert_eq!(report.outcome, JobOutcome::Cancelled);
    assert_eq!((report.removed_files, report.removed_dirs), (0, 0));

    assert_the_session_still_runs_a_command(&mut host).await;
    host.stop().expect("stop");
    drain_until_end!(host);
    drain_until_end!(joiner);
    assert_eq!(audit.of("fs_remove_tree_start"), vec![Outcome::Ok]);
    assert_eq!(audit.of("fs_remove_tree"), vec![Outcome::Cancelled]);
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

/// A removal that cannot remove everything removes the rest, names what it
/// left with the reason, and is a failure on both sides: the folder is still
/// there, so nothing reports it as removed (REQ-N-008).
#[tokio::test]
async fn a_removal_that_leaves_something_behind_is_a_failure_with_its_reasons() {
    use cretlink_session::ActionOutcome;

    let audit = Arc::new(OutcomeAudit::default());
    let (mut host, mut joiner) = connected_pair_explorer(audit.clone()).await;
    drive_to_active(&mut host, &mut joiner).await;
    let folder = staged_folder(&host, "in-use");
    plant_files(&folder, 4);
    let Some((held, kept, reason)) = Held::plant(&folder) else {
        return;
    };

    let report = host
        .remove_tree(staged("in-use"), &CancelToken::new(), |_| {})
        .await
        .expect("a partial removal is a report");

    assert_eq!(report.outcome, JobOutcome::Failed, "never a success");
    assert_eq!(report.removed_files, 4, "everything else was removed");
    assert_eq!(
        report.removed_dirs, 0,
        "the folders that hold the entry stay"
    );
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].path, kept.display().to_string());
    assert_eq!(report.failed[0].code, reason);
    assert_eq!(report.failed_more, 0);
    assert!(!report.blocked && report.error.is_none());
    assert!(kept.exists() && folder.exists());

    let (_, _, _, operation, _, _, outcome) = handled_parts(next_handled(&mut joiner).await);
    assert_eq!(operation, removal_of("in-use"));
    assert_eq!(outcome, ActionOutcome::Failed { code: reason });

    // Once the entry is free, the same call removes the rest and the folder.
    drop(held);
    let report = host
        .remove_tree(staged("in-use"), &CancelToken::new(), |_| {})
        .await
        .expect("the removal runs");
    assert_eq!(report.outcome, JobOutcome::Completed);
    assert_eq!((report.removed_files, report.removed_dirs), (1, 2));
    assert!(!folder.exists());

    host.stop().expect("stop");
    drain_until_end!(host);
    drain_until_end!(joiner);
    assert_eq!(
        audit.of("fs_remove_tree"),
        vec![Outcome::Failed, Outcome::Ok]
    );
}

/// A folder that is not there, and a file where a folder was expected, are
/// failed reports with the agent's reason. Neither is an error of the session,
/// which stays usable, and nothing is removed.
#[tokio::test]
async fn removing_what_is_not_a_folder_is_a_failed_report() {
    let (mut host, mut joiner) = connected_pair_explorer(Arc::default()).await;
    drive_to_active(&mut host, &mut joiner).await;
    let folder = staged_folder(&host, "holder");
    let file = folder.join("a-file.txt");
    std::fs::write(&file, b"a file").expect("plant");
    // The file lies directly in the staging area, where a staging name finds it.
    let staged_file = folder.with_file_name("just-a-file.txt");
    std::fs::write(&staged_file, b"a file").expect("plant");

    for (name, expected) in [
        ("not-there", ErrorCode::PathNotFound),
        ("just-a-file.txt", ErrorCode::WrongPathKind),
    ] {
        let report = host
            .remove_tree(staged(name), &CancelToken::new(), |_| {})
            .await
            .expect("a refused removal is a report");
        assert_eq!(report.outcome, JobOutcome::Failed, "{name}");
        assert_eq!(
            report.error.as_ref().map(|error| error.code),
            Some(expected),
            "{name}"
        );
        assert_eq!((report.removed_files, report.removed_dirs), (0, 0));
    }
    assert!(staged_file.exists() && file.exists(), "nothing was removed");

    assert_the_session_still_runs_a_command(&mut host).await;
    host.stop().expect("stop");
    drain_until_end!(host);
    let _ = std::fs::remove_file(&staged_file);
    let _ = std::fs::remove_dir_all(&folder);
}

/// A path that breaks the path rules never leaves the console: the call fails
/// locally with the typed reason and the session stays healthy.
#[tokio::test]
async fn an_adversarial_folder_path_is_refused_before_it_is_sent() {
    let (mut host, mut joiner) = connected_pair_explorer(Arc::default()).await;
    drive_to_active(&mut host, &mut joiner).await;

    for bad in [
        RemotePath::Absolute {
            path: "C:\\Users\\..\\Windows".to_string(),
        },
        RemotePath::Absolute {
            path: "\\\\server\\share\\folder".to_string(),
        },
        RemotePath::Staging {
            name: "..\\outside".to_string(),
        },
    ] {
        let error = host
            .remove_tree(bad.clone(), &CancelToken::new(), |_| {})
            .await
            .expect_err("an adversarial path must be refused");
        match error {
            SessionError::Invalid { code, .. } => {
                assert_eq!(code, ErrorCode::InvalidPath, "{bad:?}");
            }
            other => panic!("expected a local Invalid rejection for {bad:?}, got {other:?}"),
        }
    }

    assert_the_session_still_runs_a_command(&mut host).await;
    host.stop().expect("stop");
    drain_until_end!(host);
}

/// A removal step bound to another session is refused and removes nothing
/// (REQ-S-012, REQ-S-014).
#[tokio::test]
async fn a_removal_bound_to_another_session_removes_nothing() {
    let (mut host, mut joiner) = connected_pair_explorer(Arc::default()).await;
    drive_to_active(&mut host, &mut joiner).await;
    let folder = staged_folder(&host, "theirs");
    plant_files(&folder, 3);

    let error = host
        .file_op_with_session(
            SessionId::new(),
            Operation::RemoveTree {
                path: staged("theirs"),
                stop: false,
            },
        )
        .await
        .expect_err("a request for another session is refused");
    match error {
        SessionError::Remote { code, .. } => assert_eq!(code, ErrorCode::UnknownSession),
        other => panic!("expected UnknownSession, got {other:?}"),
    }
    assert_eq!(entries_in(&folder), 3, "nothing was removed");

    host.stop().expect("stop");
    drain_until_end!(host);
    let _ = std::fs::remove_dir_all(&folder);
}

/// A session that is revoked while a folder is being removed ends the removal:
/// the call does not report a success, no further step runs, and the agent's
/// audit trail records the removal as begun with a result that is not known,
/// never as done (REQ-S-004, REQ-N-008).
#[tokio::test]
async fn a_revoke_during_a_removal_never_reads_as_a_success() {
    use cretlink_protocol::limits::MAX_TREE_STEP_ENTRIES;

    let audit = Arc::new(OutcomeAudit::default());
    let (mut host, mut joiner) = connected_pair_explorer(audit.clone()).await;
    drive_to_active(&mut host, &mut joiner).await;
    let folder = staged_folder(&host, "interrupted");
    let files = MAX_TREE_STEP_ENTRIES + 300;
    plant_files(&folder, files);

    // The customer stops the session the moment the first step is answered.
    let mut fired = false;
    let result = host
        .remove_tree(staged("interrupted"), &CancelToken::new(), |_| {
            if !fired {
                fired = true;
                joiner.stop().expect("the customer stops the session");
            }
        })
        .await;

    assert!(fired, "the stop was fired after the first step");
    // An error of the session, or a report that is not a success.
    if let Ok(report) = &result {
        assert_ne!(report.outcome, JobOutcome::Completed, "{report:?}");
    }
    drain_until_end!(host);
    drain_until_end!(joiner);
    assert!(folder.exists(), "the removal did not run to its end");
    assert!(entries_in(&folder) > 0);
    assert_eq!(audit.of("fs_remove_tree_start"), vec![Outcome::Ok]);
    assert_eq!(
        audit.of("fs_remove_tree"),
        vec![Outcome::Unknown],
        "a removal the session cut short is not known to have ended"
    );
    let _ = std::fs::remove_dir_all(&folder);
}

/// A file is sent to a folder by its absolute path: it appears there under
/// its name, verified, and the customer sees it as a received file with its
/// path and size. An existing file is replaced only on request. Windows only:
/// the wire accepts drive-qualified paths only.
#[cfg(windows)]
#[tokio::test]
async fn a_file_is_sent_to_a_folder_and_replaced_only_on_request() {
    use cretlink_session::{ActionOutcome, ActivityKind, Origin};
    use sha2::{Digest, Sha256};

    let (mut host, mut joiner) = connected_pair_explorer(Arc::default()).await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("send-to-folder");
    let documents = dir.join("Documents");
    std::fs::create_dir_all(&documents).expect("create the folder");
    let data = windowed_payload();
    let source = dir.join("source.bin");
    std::fs::write(&source, &data).expect("write source");
    let final_path = documents.join("manual.pdf");
    let dest = RemotePath::Absolute {
        path: final_path.display().to_string(),
    };
    let send = |bytes_from: PathBuf, overwrite: bool| UploadRequest {
        source: bytes_from,
        dest: dest.clone(),
        source_label: String::new(),
        cleanup: CleanupPolicy::Keep,
        overwrite,
    };

    let report = host
        .upload_file(send(source.clone(), false), &CancelToken::new(), |_| {})
        .await
        .expect("the transfer runs");
    assert_eq!(report.outcome, JobOutcome::Completed, "{:?}", report.error);
    assert_eq!(report.digest, Sha256::digest(&data).to_vec());
    assert_eq!(std::fs::read(&final_path).expect("read"), data);
    let names: Vec<String> = std::fs::read_dir(&documents)
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, ["manual.pdf"], "no part is left beside the file");

    // The customer sees the file arrive, with its path and size.
    let received = loop {
        let parts = handled_parts(next_handled(&mut joiner).await);
        if parts.2 == ActivityKind::FileReceived {
            break parts;
        }
    };
    assert_eq!(received.0, Origin::Console);
    assert_eq!(received.4, Some(final_path.display().to_string()));
    assert_eq!(received.5, Some(data.len() as u64));
    assert_eq!(received.6, ActionOutcome::Succeeded);

    // Nothing was staged: the file is the customer's to keep.
    match host.file_op(Operation::ListStaged {}).await.expect("list") {
        OpResult::StagedList { entries } => assert!(entries.is_empty()),
        other => panic!("expected StagedList, got {other:?}"),
    }

    // An existing file is not replaced unless the caller asks for it.
    let other = dir.join("other.bin");
    std::fs::write(&other, b"another file").expect("write");
    let refused = host
        .upload_file(send(other.clone(), false), &CancelToken::new(), |_| {})
        .await
        .expect("a refused transfer is a report");
    assert_eq!(refused.outcome, JobOutcome::Failed);
    assert_eq!(
        refused.error.as_ref().map(|error| error.code),
        Some(ErrorCode::AlreadyExists)
    );
    assert_eq!(std::fs::read(&final_path).expect("read"), data, "untouched");

    let replaced = host
        .upload_file(send(other, true), &CancelToken::new(), |_| {})
        .await
        .expect("the transfer runs");
    assert_eq!(replaced.outcome, JobOutcome::Completed);
    assert_eq!(std::fs::read(&final_path).expect("read"), b"another file");

    host.stop().expect("stop");
    drain_until_end!(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A session that ends while a file is on its way to a folder leaves nothing
/// in that folder: the file never appears under its name, and the part that
/// lay beside it is removed by the agent's own cleanup (REQ-F-019,
/// REQ-N-007). Windows only, like the test above.
#[cfg(windows)]
#[tokio::test]
async fn a_revoke_during_a_transfer_to_a_folder_leaves_no_part_behind() {
    let (mut host, mut joiner) = connected_pair_explorer(Arc::default()).await;
    drive_to_active(&mut host, &mut joiner).await;

    let dir = temp_dir_for("revoke-send-to-folder");
    let folder = dir.join("Downloads");
    std::fs::create_dir_all(&folder).expect("create the folder");
    let source = dir.join("source.bin");
    std::fs::write(&source, large_payload()).expect("write source");
    let final_path = folder.join("setup.exe");
    let entries_now = || entries_in(&folder);

    let mut fired = false;
    let mut part_seen_mid = false;
    let report = host
        .upload_file(
            UploadRequest {
                source,
                dest: RemotePath::Absolute {
                    path: final_path.display().to_string(),
                },
                source_label: String::new(),
                cleanup: CleanupPolicy::Keep,
                overwrite: false,
            },
            &CancelToken::new(),
            |p: TransferProgress| {
                if !fired && p.transferred > 0 {
                    fired = true;
                    part_seen_mid = entries_now() == 1;
                    let _ = host.revoke(RevokeReason::TechnicianRequested);
                }
            },
        )
        .await;

    assert!(
        fired,
        "the revoke was fired from a mid-stream progress callback"
    );
    assert!(
        part_seen_mid,
        "the part lay beside the destination meanwhile"
    );
    // An error of the session, or a report that is not a success.
    if let Ok(report) = &report {
        assert_ne!(report.outcome, JobOutcome::Completed, "{report:?}");
    }
    drain_until_end!(host);
    drain_until_end!(joiner);
    assert!(
        !final_path.exists(),
        "the file never appeared under its name"
    );
    assert_eq!(entries_in(&folder), 0, "and its part is gone");
    let _ = std::fs::remove_dir_all(&dir);
}
