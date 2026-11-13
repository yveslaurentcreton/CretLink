//! A session through the relay in both roles (ST-M007-013).
//!
//! Both roles run here as they do in the application, without their windows:
//! the technician's state, its **Use relay** and its session task
//! ([`Technician`]); the customer's code field, its dial and its pump
//! ([`Customer`]). Between them stands the real relay server with TLS inside
//! this process ([`TestRelay`]); each copy takes it from a profile file next
//! to its program, with the relay's certificate as its authority file. Nothing
//! leaves the loopback interface.
//!
//! What is shown: a relay code leads two copies to each other and to nothing
//! else. The approval code, the technician's confirmation and the customer's
//! consent decide exactly as on one network (ADR-0016); a command and a
//! transfer run through the relay; either side stops the session; a relay
//! that stops ends it on both sides, and nothing reconnects; and every way
//! the relay can fail to be used is said in words with a next step
//! (REQ-N-008, REQ-N-015).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use cretlink_console::{ConsoleState, SessionPhase, StopReason};
use cretlink_discovery::RelayCode;
use cretlink_mcp::{DownloadParams, ExecParams, TargetError, UploadParams};
use cretlink_protocol::{CleanupPolicy, JobOutcome, RemotePath, Shell};
use cretlink_relay::{AdmissionPolicy, AdmissionToken, ClientId};
use cretlink_relay_server::ServeConfig;
use cretlink_session::{ActivityEntry, ActivityKind, EndedBy};
use cretlink_support_agent::AgentPhase;

use crate::customer::testing::Customer;
use crate::customer::{ConsentView, RelayUseView};
use crate::relay::testing::{CA_FILE, Folder, RELAY_NAME, TestRelay};
use crate::technician::testing::{STEP, Technician};

/// The key the relay's operator gave the technician's copy.
const TECHNICIAN_KEY: &str = "5f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5";
/// The key the relay's operator gave the customer's copy.
const CUSTOMER_KEY: &str = "9a41c7b35f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1";
/// A key the relay does not know.
const UNKNOWN_KEY: &str = "0000000000000000-not-a-key-of-this-relay";

/// A relay that admits the two copies by a key of their own each, as its
/// operator sets it up for a customer (ST-M007-014).
async fn keyed_relay() -> TestRelay {
    keyed_relay_with(ServeConfig::default()).await
}

async fn keyed_relay_with(config: ServeConfig) -> TestRelay {
    let policy = AdmissionPolicy::Allowlist(vec![
        (
            AdmissionToken::from_text(TECHNICIAN_KEY),
            ClientId::new("technician"),
        ),
        (
            AdmissionToken::from_text(CUSTOMER_KEY),
            ClientId::new("customer"),
        ),
    ]);
    TestRelay::start_with(policy, config).await
}

/// The technician's copy with the profile of `relay` next to its program.
fn technician_at(relay: &TestRelay, test: &str, key: Option<&str>) -> Technician {
    let technician = Technician::new(test);
    relay.set_up(technician.folder(), key);
    technician
}

/// **Use relay** in a technician copy whose profile file is next to its
/// program. Answers the waiting state the window gets back.
fn use_relay(technician: &Technician) -> ConsoleState {
    let program = technician.folder().path("cretlink");
    technician.use_relay(technician.relay_of(&program).expect("a relay profile"))
}

/// The customer's copy in a folder of its own with the profile of `relay`
/// next to its program.
fn customer_at(relay: &TestRelay, test: &str, key: Option<&str>) -> (Customer, Folder) {
    let folder = Folder::new(&format!("customer-{test}"));
    let program = relay.set_up(&folder, key);
    (Customer::at(&program), folder)
}

/// The relay code and the relay of a waiting state.
fn waiting_on(state: &ConsoleState) -> (String, String, bool) {
    let SessionPhase::Listening { code, .. } = &state.phase else {
        panic!("the console does not wait: {:?}", state.phase);
    };
    let relay = state.relay.as_ref().expect("the console waits on a relay");
    (code.clone(), relay.host.clone(), relay.admitted)
}

fn relay_holds_the_code(state: &ConsoleState) -> bool {
    matches!(state.phase, SessionPhase::Listening { .. })
        && state.relay.as_ref().is_some_and(|relay| relay.admitted)
}

fn through_the_test_relay() -> Option<RelayUseView> {
    Some(RelayUseView {
        host: RELAY_NAME.to_owned(),
    })
}

fn relay_of(state: &ConsoleState) -> Option<&str> {
    state.relay.as_ref().map(|relay| relay.host.as_str())
}

fn failed_at(view: &ConsentView) -> Option<(String, String)> {
    match &view.status.phase {
        AgentPhase::Failed { stage, message } => Some((stage.clone(), message.clone())),
        _ => None,
    }
}

fn console_failure(state: &ConsoleState) -> Option<(String, String, Option<String>)> {
    match &state.phase {
        SessionPhase::Failed {
            stage,
            message,
            next,
        } => Some((stage.clone(), message.clone(), next.clone())),
        _ => None,
    }
}

/// From **Use relay** to full control: the technician waits on the relay, the
/// customer types the code, both compare the approval code, the technician
/// confirms and the customer allows. Answers the relay code that was read
/// out.
async fn pair(technician: &Technician, customer: &mut Customer) -> String {
    let waiting = use_relay(technician);
    let (code, host, _) = waiting_on(&waiting);
    assert_eq!(host, RELAY_NAME);
    technician
        .wait_for("that the relay holds the code", relay_holds_the_code)
        .await;

    customer.connect(&code).expect("the code is taken");
    let on_console = technician
        .wait_for("the approval code", |state| {
            matches!(state.phase, SessionPhase::AwaitingApprovalCode { .. })
        })
        .await;
    let on_customer = customer
        .wait_for("the approval code", |view| {
            view.approval_code.is_some()
                && matches!(view.status.phase, AgentPhase::AwaitingApprovalCompare)
        })
        .await;
    let SessionPhase::AwaitingApprovalCode { code: approval } = &on_console.phase else {
        unreachable!();
    };
    assert_eq!(
        on_customer.approval_code.as_deref(),
        Some(approval.as_str())
    );

    technician.confirm();
    customer
        .wait_for("the decision", |view| {
            matches!(view.status.phase, AgentPhase::AwaitingConsent)
        })
        .await;
    customer.allow();
    technician
        .wait_for("full control", |state| {
            matches!(state.phase, SessionPhase::FullControl)
        })
        .await;
    customer
        .wait_for("full control", |view| view.status.control_active)
        .await;
    code
}

fn echo(marker: &str) -> ExecParams {
    ExecParams {
        shell: Shell::Cmd,
        command: format!("echo {marker}"),
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: Some(30_000),
        sensitive: false,
    }
}

// --- From the code to the end ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_code_leads_to_a_session_like_on_one_network_and_the_technician_stops_it() {
    let relay = keyed_relay().await;
    let technician = technician_at(&relay, "lifecycle", Some(TECHNICIAN_KEY));
    let (mut customer, _customer_folder) = customer_at(&relay, "lifecycle", Some(CUSTOMER_KEY));

    // Use relay: the waiting state at once, with the relay by its host name
    // and a relay code. No target, no listener, no authority.
    let waiting = use_relay(&technician);
    let (code, host, _) = waiting_on(&waiting);
    assert_eq!(host, RELAY_NAME);
    let SessionPhase::Listening { address, .. } = &waiting.phase else {
        unreachable!();
    };
    assert_eq!(address, &relay.addr.to_string());
    assert!(RelayCode::parse(&code).is_ok(), "{code}");
    assert_eq!(code.len(), 19, "four groups of four: {code}");
    assert!(waiting.targets.is_empty());
    assert!(!waiting.can_issue_actions());
    assert!(!technician.has_control());
    // The relay is said to hold the code only once it does.
    technician
        .wait_for("that the relay holds the code", relay_holds_the_code)
        .await;

    // The customer types the code as people type what is read to them.
    let typed = code.to_lowercase().replace('-', " ");
    let reaching = customer.connect(&typed).expect("the code is taken");
    // From the first moment the window says which relay this goes through.
    assert_eq!(reaching.relay, through_the_test_relay());
    assert!(reaching.session_active);
    assert!(!reaching.status.control_active);

    // Both sides show the same approval code, and it is not the relay code:
    // the relay code only led the two copies to each other.
    let on_console = technician
        .wait_for("the approval code", |state| {
            matches!(state.phase, SessionPhase::AwaitingApprovalCode { .. })
        })
        .await;
    let on_customer = customer
        .wait_for("the approval code", |view| {
            view.approval_code.is_some()
                && matches!(view.status.phase, AgentPhase::AwaitingApprovalCompare)
        })
        .await;
    let SessionPhase::AwaitingApprovalCode { code: approval } = &on_console.phase else {
        unreachable!();
    };
    assert_eq!(
        on_customer.approval_code.as_deref(),
        Some(approval.as_str())
    );
    assert_ne!(approval, &code);
    assert_eq!(relay_of(&on_console), Some(RELAY_NAME));
    assert_eq!(on_customer.relay, through_the_test_relay());
    // Connected through the relay, and nobody has control: the relay grants
    // nothing, and no relay is offered again while a customer is connected.
    assert!(!on_console.can_issue_actions());
    assert!(!technician.has_control());
    assert!(!on_customer.status.control_active);
    assert!(!technician.relay_can_be_chosen());

    // The technician confirms; still no control, the customer decides.
    technician.confirm();
    let deciding = customer
        .wait_for("the decision", |view| {
            matches!(view.status.phase, AgentPhase::AwaitingConsent)
        })
        .await;
    assert!(!deciding.status.control_active);
    technician
        .wait_for("that the customer decides", |state| {
            matches!(state.phase, SessionPhase::AwaitingConsent)
        })
        .await;
    assert!(!technician.has_control());

    customer.allow();
    let live = technician
        .wait_for("full control", |state| {
            matches!(state.phase, SessionPhase::FullControl)
        })
        .await;
    let under_control = customer
        .wait_for("full control", |view| view.status.control_active)
        .await;
    assert!(live.can_issue_actions());
    assert_eq!(relay_of(&live), Some(RELAY_NAME));
    assert_eq!(under_control.relay, through_the_test_relay());

    // A command, through the relay.
    let service = technician.service().await;
    let done = tokio::time::timeout(STEP, service.exec(None, echo("relayed-7f3a"), 64 * 1024))
        .await
        .expect("the command ends")
        .expect("the command runs");
    assert_eq!(done.outcome, JobOutcome::Completed, "{done:?}");
    assert!(
        String::from_utf8_lossy(&done.stdout).contains("relayed-7f3a"),
        "{done:?}"
    );

    // A transfer there and back, through the relay.
    let content = b"A note that travels through the relay and comes back unchanged.\n".repeat(64);
    let sent = technician.folder().write("to-send.txt", &content);
    let up = tokio::time::timeout(
        STEP,
        service.upload(
            None,
            UploadParams {
                local_path: sent.display().to_string(),
                dest: RemotePath::Staging {
                    name: "relayed-note-7f3a.txt".to_owned(),
                },
                source_label: "to-send.txt".to_owned(),
                cleanup: CleanupPolicy::OnSessionEnd,
                overwrite: true,
            },
        ),
    )
    .await
    .expect("the upload ends")
    .expect("the upload runs");
    assert_eq!(up.outcome, JobOutcome::Completed, "{up:?}");
    assert_eq!(up.len, content.len() as u64);
    let back = technician.folder().path("came-back.txt");
    let down = tokio::time::timeout(
        STEP,
        service.download(
            None,
            DownloadParams {
                source: RemotePath::Staging {
                    name: "relayed-note-7f3a.txt".to_owned(),
                },
                local_path: back.display().to_string(),
            },
        ),
    )
    .await
    .expect("the download ends")
    .expect("the download runs");
    assert_eq!(down.outcome, JobOutcome::Completed, "{down:?}");
    assert_eq!(std::fs::read(&back).unwrap(), content);
    assert_eq!(down.digest, up.digest);

    // The customer saw each of them, as on one network.
    let kinds: Vec<ActivityKind> = customer
        .activity()
        .into_iter()
        .filter_map(|entry| match entry {
            ActivityEntry::Item { item } => Some(item.kind),
            ActivityEntry::Gap { .. } => None,
        })
        .collect();
    assert!(kinds.contains(&ActivityKind::Command), "{kinds:?}");
    assert!(kinds.contains(&ActivityKind::FileReceived), "{kinds:?}");
    assert!(kinds.contains(&ActivityKind::FileSent), "{kinds:?}");

    // The technician stops; both sides end for that reason.
    technician.stop().expect("stop");
    let ended = customer
        .wait_for("the end", |view| {
            matches!(view.status.phase, AgentPhase::Stopped { .. })
        })
        .await;
    assert!(
        matches!(
            ended.status.phase,
            AgentPhase::Stopped {
                reason: cretlink_support_agent::StopReason::TechnicianRequested
            }
        ),
        "{:?}",
        ended.status.phase
    );
    assert!(!ended.status.control_active);
    // The end still says which way the session went.
    assert_eq!(ended.relay, through_the_test_relay());
    technician.finished().await;
    customer.settled().await;
    let console = technician.console();
    assert_eq!(
        console.phase,
        SessionPhase::Stopped {
            reason: StopReason::TechnicianRequested
        }
    );
    assert_eq!(relay_of(&console), Some(RELAY_NAME));
    assert!(!technician.has_control());
    // A stale handle to the ended session does nothing.
    assert!(matches!(
        service.exec(None, echo("after-the-end"), 1024).await,
        Err(TargetError::NoConsentedSession)
    ));

    // The audit records the relay; the session's history names it.
    let summary = technician.sealed_session().await;
    assert_eq!(summary.relay.as_deref(), Some(RELAY_NAME));
    assert_eq!(summary.ended_by, Some(EndedBy::Technician));
    let trail = technician.audit_text();
    assert!(trail.contains("session_route"), "{trail}");
    assert!(trail.contains("consent_granted"), "{trail}");
    // Neither a key nor the relay code is in the trail.
    for secret in [TECHNICIAN_KEY, CUSTOMER_KEY, &code, &code.replace('-', "")] {
        assert!(!trail.contains(secret), "the audit trail holds {secret}");
    }

    // The relay passed the session on and saw nothing of it but its size.
    let metrics = relay.control.metrics();
    assert!(
        metrics.bytes_forwarded > content.len() as u64,
        "{metrics:?}"
    );
    assert_eq!(metrics.denied_auth, 0, "{metrics:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_customer_stops_a_session_through_the_relay() {
    let relay = keyed_relay().await;
    let technician = technician_at(&relay, "customer-stops", Some(TECHNICIAN_KEY));
    let (mut customer, _folder) = customer_at(&relay, "customer-stops", Some(CUSTOMER_KEY));
    pair(&technician, &mut customer).await;
    technician.service().await;
    assert!(technician.has_control());

    customer.stop().expect("stop");
    let ended = technician
        .wait_for("the end", |state| {
            matches!(state.phase, SessionPhase::Stopped { .. })
        })
        .await;
    assert_eq!(
        ended.phase,
        SessionPhase::Stopped {
            reason: StopReason::CustomerRequested
        }
    );
    assert_eq!(relay_of(&ended), Some(RELAY_NAME));
    technician.finished().await;
    customer.settled().await;
    assert!(!technician.has_control());
    let view = customer.view();
    assert!(!view.status.control_active);
    assert!(!view.session_active);
    assert_eq!(view.relay, through_the_test_relay());
    let summary = technician.sealed_session().await;
    assert_eq!(summary.ended_by, Some(EndedBy::Customer));
    assert_eq!(summary.relay.as_deref(), Some(RELAY_NAME));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_stops_in_the_middle_ends_the_session_on_both_sides() {
    let relay = keyed_relay().await;
    let technician = technician_at(&relay, "relay-stops", Some(TECHNICIAN_KEY));
    let (mut customer, _folder) = customer_at(&relay, "relay-stops", Some(CUSTOMER_KEY));
    pair(&technician, &mut customer).await;
    let service = technician.service().await;

    // The relay's operator stops it while the session is under full control.
    relay.stop();

    // Both sides end, fail closed, as for any lost connection; and each can
    // still say that the session ran through the relay, so the end is not a
    // lost session of unknown cause.
    let console = technician
        .wait_for("the end", |state| {
            matches!(
                state.phase,
                SessionPhase::Stopped { .. } | SessionPhase::Failed { .. }
            )
        })
        .await;
    assert_eq!(
        console.phase,
        SessionPhase::Stopped {
            reason: StopReason::Disconnected
        }
    );
    assert_eq!(relay_of(&console), Some(RELAY_NAME));
    assert!(!console.can_issue_actions());
    let view = customer
        .wait_for("the end", |view| {
            matches!(
                view.status.phase,
                AgentPhase::Stopped { .. } | AgentPhase::Failed { .. }
            )
        })
        .await;
    assert!(
        matches!(
            view.status.phase,
            AgentPhase::Stopped {
                reason: cretlink_support_agent::StopReason::Disconnected
            }
        ),
        "{:?}",
        view.status.phase
    );
    assert!(!view.status.control_active);
    assert_eq!(view.relay, through_the_test_relay());

    // Nothing reconnects by itself: both tasks have ended, no session exists,
    // and the handle of the lost session does nothing.
    technician.finished().await;
    customer.settled().await;
    assert!(!technician.has_control());
    assert!(!customer.view().session_active);
    assert!(matches!(
        service
            .exec(None, echo("after-the-relay-stopped"), 1024)
            .await,
        Err(TargetError::NoConsentedSession)
    ));
    assert_eq!(
        technician.console().phase,
        SessionPhase::Stopped {
            reason: StopReason::Disconnected
        }
    );
    // The history says that the connection was lost, and through which relay.
    let summary = technician.sealed_session().await;
    assert_eq!(summary.ended_by, Some(EndedBy::ConnectionLost));
    assert_eq!(summary.relay.as_deref(), Some(RELAY_NAME));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_stops_before_the_customer_decided_is_said_with_the_relay() {
    let relay = keyed_relay().await;
    let technician = technician_at(&relay, "relay-stops-early", Some(TECHNICIAN_KEY));
    let (mut customer, _folder) = customer_at(&relay, "relay-stops-early", Some(CUSTOMER_KEY));
    let (code, _, _) = waiting_on(&use_relay(&technician));
    technician
        .wait_for("that the relay holds the code", relay_holds_the_code)
        .await;
    customer.connect(&code).expect("the code is taken");
    technician
        .wait_for("the approval code", |state| {
            matches!(state.phase, SessionPhase::AwaitingApprovalCode { .. })
        })
        .await;
    customer
        .wait_for("the approval code", |view| view.approval_code.is_some())
        .await;

    // The relay stops while both are comparing the approval code.
    relay.stop();

    // Neither side reads as a failure of unknown cause: both name the relay.
    let console = technician
        .wait_for("the end", |state| console_failure(state).is_some())
        .await;
    let (stage, message, _) = console_failure(&console).unwrap();
    assert_eq!(stage, "relay_lost");
    assert!(message.contains(RELAY_NAME), "{message}");
    assert_eq!(relay_of(&console), Some(RELAY_NAME));
    assert!(!console.can_issue_actions());
    let view = customer
        .wait_for("the end", |view| failed_at(view).is_some())
        .await;
    let (stage, message) = failed_at(&view).unwrap();
    assert_eq!(stage, "relay_lost");
    assert!(message.contains(RELAY_NAME), "{message}");
    assert!(!view.status.control_active);
    assert!(!view.session_active);
    technician.finished().await;
    customer.settled().await;
    assert!(!technician.has_control());
}

// --- The code ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_that_nobody_waits_on_is_said_and_a_code_leads_to_one_session_only() {
    let relay = keyed_relay().await;
    let limit = Some(Duration::from_millis(400));

    // Nobody waits on this code: it is mistyped, or the technician stopped.
    let (mut nobody, _nobody_folder) = customer_at(&relay, "nobody", Some(CUSTOMER_KEY));
    nobody
        .connect_within("53ST-A71D-JKKV-0VGH", limit)
        .expect("a well-formed relay code is taken");
    let view = nobody
        .wait_for("that nobody waits on the code", |view| {
            failed_at(view).is_some()
        })
        .await;
    let (stage, _) = failed_at(&view).unwrap();
    assert_eq!(stage, "rendezvous");
    assert_eq!(view.relay, through_the_test_relay());
    assert!(!view.session_active);
    nobody.settled().await;

    // A code leads to one session: once the customer is connected, the same
    // code leads a second copy to nobody.
    let technician = technician_at(&relay, "once", Some(TECHNICIAN_KEY));
    let (code, _, _) = waiting_on(&use_relay(&technician));
    technician
        .wait_for("that the relay holds the code", relay_holds_the_code)
        .await;
    let (mut first, _first_folder) = customer_at(&relay, "once-first", Some(CUSTOMER_KEY));
    first.connect(&code).expect("the code is taken");
    technician
        .wait_for("the approval code", |state| {
            matches!(state.phase, SessionPhase::AwaitingApprovalCode { .. })
        })
        .await;
    let (mut second, _second_folder) = customer_at(&relay, "once-second", Some(CUSTOMER_KEY));
    second
        .connect_within(&code, limit)
        .expect("the code is well formed");
    let late = second
        .wait_for("that the code was used", |view| failed_at(view).is_some())
        .await;
    assert_eq!(failed_at(&late).unwrap().0, "rendezvous");
    second.settled().await;
    // The session of the first is undisturbed.
    assert!(matches!(
        technician.console().phase,
        SessionPhase::AwaitingApprovalCode { .. }
    ));
    assert!(first.view().approval_code.is_some());

    technician.stop().expect("stop");
    technician.finished().await;
    first.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_ends_the_wait_and_every_wait_has_a_code_of_its_own() {
    let relay = keyed_relay().await;
    let technician = technician_at(&relay, "cancel", Some(TECHNICIAN_KEY));
    let (first_code, _, _) = waiting_on(&use_relay(&technician));
    technician
        .wait_for("that the relay holds the code", relay_holds_the_code)
        .await;

    // Cancel (Escape): the wait ends at once and there is no session.
    let cancelled = technician.stop().expect("cancel");
    assert_eq!(
        cancelled.phase,
        SessionPhase::Stopped {
            reason: StopReason::TechnicianRequested
        }
    );
    technician.finished().await;
    assert!(technician.sessions().is_empty());

    // The code leads to no session any more.
    let (mut customer, _folder) = customer_at(&relay, "cancel", Some(CUSTOMER_KEY));
    customer
        .connect_within(&first_code, Some(Duration::from_millis(400)))
        .expect("the code is well formed");
    let view = customer
        .wait_for("that nobody waits", |view| failed_at(view).is_some())
        .await;
    assert_eq!(failed_at(&view).unwrap().0, "rendezvous");
    customer.settled().await;
    // The cancelled wait stayed cancelled: nothing woke it.
    assert_eq!(technician.console().phase, cancelled.phase);

    // The next wait has a new code, and that one works.
    let (second_code, _, _) = waiting_on(&use_relay(&technician));
    assert_ne!(second_code, first_code);
    technician
        .wait_for("that the relay holds the new code", relay_holds_the_code)
        .await;
    customer.connect(&second_code).expect("the code is taken");
    technician
        .wait_for("the approval code", |state| {
            matches!(state.phase, SessionPhase::AwaitingApprovalCode { .. })
        })
        .await;
    technician.stop().expect("stop");
    technician.finished().await;
    customer.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wait_outlives_a_rendezvous_that_the_relay_let_lapse() {
    // A relay that keeps a waiting connection for a moment only.
    let relay = keyed_relay_with(ServeConfig {
        rendezvous_timeout: Duration::from_millis(250),
        ..ServeConfig::default()
    })
    .await;
    let technician = technician_at(&relay, "lapse", Some(TECHNICIAN_KEY));
    let program = technician.folder().path("cretlink");
    let waiting = technician.use_relay_redialling_after(
        technician.relay_of(&program).expect("a relay profile"),
        Duration::from_millis(50),
    );
    let (code, _, _) = waiting_on(&waiting);

    // The relay lets the rendezvous lapse, more than once; the technician's
    // copy takes the code back to the relay every time, and the screen keeps
    // waiting on the same code.
    let lapsed = async {
        while relay.control.metrics().sessions_admitted < 4 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    tokio::time::timeout(STEP, lapsed)
        .await
        .expect("the waiting copy dials again after a lapse");
    let still = technician.console();
    assert_eq!(waiting_on(&still).0, code);
    assert!(relay_holds_the_code(&still));

    // The customer types the code minutes later, as it were: it still works.
    let (mut customer, _folder) = customer_at(&relay, "lapse", Some(CUSTOMER_KEY));
    customer.connect(&code).expect("the code is taken");
    technician
        .wait_for("the approval code", |state| {
            matches!(state.phase, SessionPhase::AwaitingApprovalCode { .. })
        })
        .await;
    customer
        .wait_for("the approval code", |view| view.approval_code.is_some())
        .await;
    technician.stop().expect("stop");
    technician.finished().await;
    customer.settled().await;
}

// --- A relay that cannot be used ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_the_relay_does_not_know_is_said_on_either_side() {
    let relay = keyed_relay().await;

    // The technician's copy holds a key the relay does not know.
    let technician = technician_at(&relay, "key-technician", Some(UNKNOWN_KEY));
    let (code, _, _) = waiting_on(&use_relay(&technician));
    let failed = technician
        .wait_for("that the key is refused", |state| {
            console_failure(state).is_some()
        })
        .await;
    let (stage, message, next) = console_failure(&failed).unwrap();
    assert_eq!(stage, "relay_key_refused");
    assert!(message.contains(RELAY_NAME), "{message}");
    let next = next.expect("a next step");
    assert!(next.contains("key"), "{next}");
    // Where the relay of this copy is set: its profile file.
    assert!(next.contains("cretlink.toml"), "{next}");
    assert_eq!(relay_of(&failed), Some(RELAY_NAME));
    technician.finished().await;
    assert!(technician.sessions().is_empty());
    // Nothing that was shown repeats the key, and the wait is over: its code
    // is no longer on screen.
    let shown = format!("{:?}", technician.rendered());
    assert!(!shown.contains(UNKNOWN_KEY), "{shown}");
    assert!(!matches!(
        technician.console().phase,
        SessionPhase::Listening { .. }
    ));

    // The customer's copy holds a key the relay does not know.
    let (mut customer, _folder) = customer_at(&relay, "key-customer", Some(UNKNOWN_KEY));
    customer.connect(&code).expect("the code is well formed");
    let view = customer
        .wait_for("that the key is refused", |view| failed_at(view).is_some())
        .await;
    let (stage, message) = failed_at(&view).unwrap();
    assert_eq!(stage, "relay_key_refused");
    assert!(message.contains(RELAY_NAME), "{message}");
    let next = view.failure_next.clone().expect("a next step");
    assert!(next.contains("technician"), "{next}");
    assert_eq!(view.relay, through_the_test_relay());
    customer.settled().await;
    let shown = format!("{:?}", customer.rendered());
    assert!(!shown.contains(UNKNOWN_KEY), "{shown}");
    assert_eq!(relay.control.metrics().denied_auth, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_certificate_that_is_not_trusted_is_said_on_either_side_and_nothing_is_sent() {
    let relay = keyed_relay().await;
    // Each copy checks the relay against the certificate of another relay.
    let other = TestRelay::start(AdmissionPolicy::Open).await;

    let technician = technician_at(&relay, "untrusted-technician", Some(TECHNICIAN_KEY));
    technician.folder().write(CA_FILE, &other.cert_pem);
    let (code, _, _) = waiting_on(&use_relay(&technician));
    let failed = technician
        .wait_for("that the certificate is not trusted", |state| {
            console_failure(state).is_some()
        })
        .await;
    let (stage, message, next) = console_failure(&failed).unwrap();
    assert_eq!(stage, "relay_untrusted");
    assert!(message.contains("trusts"), "{message}");
    assert!(next.is_some());
    technician.finished().await;

    let (mut customer, folder) = customer_at(&relay, "untrusted-customer", Some(CUSTOMER_KEY));
    folder.write(CA_FILE, &other.cert_pem);
    // The profile is read when the window starts: start it again.
    let mut customer_again = Customer::at(&folder.path("CretLink-Support.exe"));
    std::mem::swap(&mut customer, &mut customer_again);
    customer.connect(&code).expect("the code is well formed");
    let view = customer
        .wait_for("that the certificate is not trusted", |view| {
            failed_at(view).is_some()
        })
        .await;
    assert_eq!(failed_at(&view).unwrap().0, "relay_untrusted");
    assert!(view.failure_next.is_some());
    customer.settled().await;

    // Neither copy told the relay anything: no key was presented, so the
    // relay neither admitted nor refused anybody.
    let metrics = relay.control.metrics();
    assert_eq!(
        (metrics.sessions_admitted, metrics.denied_auth),
        (0, 0),
        "{metrics:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_cannot_be_reached_is_said_on_either_side() {
    let relay = keyed_relay().await;
    let technician = technician_at(&relay, "gone-technician", Some(TECHNICIAN_KEY));
    let (customer_copy, _folder) = customer_at(&relay, "gone-customer", Some(CUSTOMER_KEY));
    let mut customer = customer_copy;
    // The relay is gone before either copy reaches it.
    drop(relay);
    tokio::task::yield_now().await;

    let (code, _, _) = waiting_on(&use_relay(&technician));
    let failed = technician
        .wait_for("that the relay cannot be reached", |state| {
            console_failure(state).is_some()
        })
        .await;
    let (stage, message, next) = console_failure(&failed).unwrap();
    assert_eq!(stage, "relay_unreachable");
    assert!(
        message.contains("could not connect to the relay"),
        "{message}"
    );
    assert!(next.expect("a next step").contains("running"));
    technician.finished().await;
    // Nothing is tried again by itself: the failure stays what it is.
    assert_eq!(technician.console().phase, failed.phase);

    customer.connect(&code).expect("the code is well formed");
    let view = customer
        .wait_for("that the relay cannot be reached", |view| {
            failed_at(view).is_some()
        })
        .await;
    assert_eq!(failed_at(&view).unwrap().0, "relay_unreachable");
    assert!(view.failure_next.is_some());
    customer.settled().await;
}

// --- LAN first ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_on_one_network_is_what_it_was_in_copies_that_have_a_relay() {
    // Both copies have a relay profile; the relay it names is not even up.
    let relay = keyed_relay().await;
    let technician = technician_at(&relay, "lan", Some(TECHNICIAN_KEY));
    let (mut customer, _folder) = customer_at(&relay, "lan", Some(CUSTOMER_KEY));
    drop(relay);

    // Start a session: the technician waits on a network of this computer.
    technician.listen_on(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let waiting = technician
        .wait_for("the connection code", |state| {
            matches!(state.phase, SessionPhase::Listening { .. })
        })
        .await;
    assert_eq!(waiting.relay, None);
    // Here, and only here, the waiting screen offers the relay.
    assert!(technician.relay_can_be_chosen());
    let SessionPhase::Listening { code, .. } = &waiting.phase else {
        unreachable!();
    };

    // The customer types the connection code into the same field.
    let reaching = customer.connect(code).expect("the code is taken");
    assert_eq!(reaching.relay, None);
    let on_console = technician
        .wait_for("the approval code", |state| {
            matches!(state.phase, SessionPhase::AwaitingApprovalCode { .. })
        })
        .await;
    let on_customer = customer
        .wait_for("the approval code", |view| view.approval_code.is_some())
        .await;
    let SessionPhase::AwaitingApprovalCode { code: approval } = &on_console.phase else {
        unreachable!();
    };
    assert_eq!(
        on_customer.approval_code.as_deref(),
        Some(approval.as_str())
    );
    assert_eq!(on_console.relay, None);
    assert_eq!(on_customer.relay, None);

    technician.stop().expect("stop");
    technician.finished().await;
    customer.settled().await;
    // The history of a session on one network names no relay.
    for summary in technician.sessions() {
        assert_eq!(summary.relay, None);
    }
}
