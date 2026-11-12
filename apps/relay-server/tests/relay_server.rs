//! ST-M007-007 — the relay-server binary forwards a **real** end-to-end CretLink
//! session and accounts for it honestly.
//!
//! Where [`crates/session/tests/over_relay.rs`] proves the portable
//! [`cretlink_relay::splice`] preserves the trust model, this suite proves the
//! same across the **actual `serve` composition** this crate ships: a real
//! [`TcpListener`], the rendezvous-preamble read, [`AdmissionControl`] admission,
//! [`Rendezvous`] matching on a single-use ticket, the splice under the guard's
//! limits, and [`cretlink_relay::SessionGuard::finish`] — end to end.
//!
//! Two dialing styles reach the real `serve` here. The three original cases
//! drive the endpoints through two tiny transparent shims — each dials the relay,
//! writes the preamble with the shared ticket, and then pipes the endpoint's raw
//! Noise bytes across — so a session that pairs through them proves `serve`
//! itself is opaque, independently of any endpoint code. The fourth case
//! exercises the **real** ST-M007-008 endpoint dialer ([`RelayDial`]) on both
//! ends: the console hosts over an outbound dial and the agent joins over its
//! own, each writing the preamble from inside the production runtime, and asserts
//! the observable trust model is byte-for-byte identical to a direct LAN pairing.
//!
//! The suite asserts four things about the real server:
//!
//! 1. **Fidelity** — a full attended lifecycle (SAC match, pre-consent refusal,
//!    consent with honest identity, an `Origin::Mcp` command, technician
//!    revocation) survives the real `serve` unchanged, and the metadata-only
//!    metrics are honest: two legs admitted, two ended, none left active, bytes
//!    forwarded, nothing denied.
//! 2. **Denial fails closed** — a wrong admission credential is denied at
//!    authentication and forwards zero bytes.
//! 3. **Lone arrival fails closed** — a single leg whose peer never comes times
//!    out and is recorded as an honest unknown end, its slot released.
//! 4. **Real-dialer parity** — the production [`RelayDial`] on both endpoints
//!    pairs a full session through the real `serve`, with the trust model
//!    identical to a direct pairing and honest metrics (ST-M007-008, REQ-F-005).
//!
//! [`crates/session/tests/over_relay.rs`]: ../../../crates/session/tests/over_relay.rs

// This test binary follows the crate e2e style: `unwrap`/`expect`/`panic` are the
// assertion vocabulary, allowed so shared driver/helper code is covered too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use cretlink_discovery::{ManualEntry, RelayCode, parse_entry};
use cretlink_protocol::{Origin, PROTOCOL_V1, TargetIdentity};
use cretlink_relay::{
    AdmissionControl, AdmissionPolicy, AdmissionToken, ClientId, ControlConfig, MetricsSnapshot,
    PREAMBLE_MAGIC, PREAMBLE_VERSION, Preamble, RENDEZVOUS_TICKET_LEN, RateLimit, RelayAnswer,
    RelayRefusal, Rendezvous, RendezvousTicket, SessionQuota, StopSignal, read_answer,
    write_preamble,
};
use cretlink_relay_server::{Leg, ServeConfig, serve};
use cretlink_session::{
    ConsoleHints, Dial, ExecRequest, HostHandle, JobId, JobOutcome, JoinHandle, NoAudit,
    NoRebootResume, RelayDial, RelayFailure, RevokeReason, SessionError, SessionEvent, SessionHost,
    SessionJoiner, Shell, WaitingDial, rendezvous_lapsed,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// Per-event wait budget: localhost pairing/consent/exec is sub-millisecond, so
/// this only guards against a hang.
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

/// Drains a handle's event stream until its terminal [`SessionEvent::Revoked`],
/// returning the reason. Tolerant of any events that precede it.
macro_rules! drain_until_revoked {
    ($handle:expr) => {{
        let mut reason = None;
        loop {
            match timeout(STEP_TIMEOUT, $handle.next_event()).await {
                Ok(Some(SessionEvent::Revoked { reason: r })) => reason = Some(r),
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => panic!("timed out draining terminal Revoked event"),
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
        display_name: None,
    }
}

/// The honest identity the agent binds into its consent grant.
fn agent_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "CUSTOMER-PC".to_string(),
        os_version: "Windows 11 Pro".to_string(),
        arch: "x86_64".to_string(),
        elevated: false,
        simulated: false,
        agent_version: "0.0.0".to_string(),
        account: None,
    }
}

/// The shared capability set both ends hash into the Noise prologue.
fn caps() -> Vec<String> {
    vec!["exec".to_string(), "files".to_string()]
}

/// Stands up the **real** relay server on a loopback port and returns its
/// address, a handle to read its live metrics, the [`StopSignal`] that must be
/// kept alive (dropping it fails the server closed), and the serve task.
async fn start_relay(
    policy: AdmissionPolicy,
    config: ServeConfig,
) -> (
    SocketAddr,
    AdmissionControl,
    StopSignal,
    tokio::task::JoinHandle<()>,
) {
    start_relay_with(policy, ControlConfig::public_default(), config).await
}

/// Like [`start_relay`], with the admission posture of `control_config`, for the
/// tests that need a tight rate limit or quota.
async fn start_relay_with(
    policy: AdmissionPolicy,
    control_config: ControlConfig,
    config: ServeConfig,
) -> (
    SocketAddr,
    AdmissionControl,
    StopSignal,
    tokio::task::JoinHandle<()>,
) {
    let control = AdmissionControl::new(policy, control_config);
    let rendezvous: Arc<Rendezvous<Leg<TcpStream>>> = Arc::new(Rendezvous::new());
    let listener = TcpListener::bind(loopback()).await.expect("bind relay");
    let addr = listener.local_addr().expect("relay addr");
    let (signal, stop) = StopSignal::new();
    let task = tokio::spawn(serve(listener, control.clone(), rendezvous, config, stop));
    (addr, control, signal, task)
}

/// The console-side stand-in for the ST-M007-008 endpoint dialer: dials the relay
/// outbound, writes the rendezvous preamble, then dials the console's real
/// listener and transparently pipes the raw Noise bytes between the two. It reads
/// and writes only opaque bytes — it is a placeholder for a dialer, not part of
/// the trusted path.
fn spawn_console_leg(
    relay_addr: SocketAddr,
    console_addr: SocketAddr,
    token: Option<AdmissionToken>,
    ticket: RendezvousTicket,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut relay = TcpStream::connect(relay_addr)
            .await
            .expect("console leg dials relay");
        write_preamble(&mut relay, &Preamble::new(token, ticket))
            .await
            .expect("console leg writes preamble");
        expect_admitted(&mut relay).await;
        let mut console = TcpStream::connect(console_addr)
            .await
            .expect("console leg dials console");
        // Transparent pipe; ends when either side closes (session teardown).
        let _ = copy_bidirectional(&mut relay, &mut console).await;
    })
}

/// The agent-side stand-in for the ST-M007-008 endpoint dialer. The real
/// [`SessionJoiner`] dials an address, so this shim owns a loopback listener the
/// joiner targets: on accept it dials the relay, writes the preamble with the
/// shared ticket, and transparently pipes the joiner's raw Noise bytes across.
async fn spawn_agent_leg(
    relay_addr: SocketAddr,
    token: Option<AdmissionToken>,
    ticket: RendezvousTicket,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(loopback()).await.expect("bind agent leg");
    let addr = listener.local_addr().expect("agent leg addr");
    let task = tokio::spawn(async move {
        let (mut joiner, _peer) = listener.accept().await.expect("agent leg accepts joiner");
        let mut relay = TcpStream::connect(relay_addr)
            .await
            .expect("agent leg dials relay");
        write_preamble(&mut relay, &Preamble::new(token, ticket))
            .await
            .expect("agent leg writes preamble");
        expect_admitted(&mut relay).await;
        let _ = copy_bidirectional(&mut joiner, &mut relay).await;
    });
    (addr, task)
}

/// Reads the relay's answer to the preamble just written (ST-M007-012).
async fn answer_of(relay: &mut TcpStream) -> RelayAnswer {
    timeout(STEP_TIMEOUT, read_answer(relay))
        .await
        .expect("the relay answers the preamble promptly")
        .expect("a well-formed answer")
}

/// Asserts that the relay admitted the leg: what follows is the peer's stream.
async fn expect_admitted(relay: &mut TcpStream) {
    assert_eq!(answer_of(relay).await, RelayAnswer::Admitted);
}

/// Dials the relay, writes a preamble with `token` and `ticket`, and returns
/// the connection with the relay's answer.
async fn dial_with(
    relay_addr: SocketAddr,
    token: Option<&str>,
    ticket: u8,
) -> (TcpStream, RelayAnswer) {
    let mut relay = TcpStream::connect(relay_addr).await.expect("dial relay");
    let preamble = Preamble::new(
        token.map(AdmissionToken::from_text),
        RendezvousTicket::from_bytes([ticket; RENDEZVOUS_TICKET_LEN]),
    );
    write_preamble(&mut relay, &preamble)
        .await
        .expect("write preamble");
    let answer = answer_of(&mut relay).await;
    (relay, answer)
}

/// Asserts that the relay closed `relay` without forwarding a byte. A relay
/// that closes while the endpoint's bytes are still unread resets the
/// connection, which is a close as well.
async fn expect_closed(relay: &mut TcpStream) {
    let mut buf = [0u8; 1];
    let read = timeout(STEP_TIMEOUT, relay.read(&mut buf))
        .await
        .expect("the relay closes the connection promptly");
    match read {
        Ok(n) => assert_eq!(n, 0, "the connection is closed with nothing forwarded"),
        Err(error) => assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ),
            "unexpected read error: {error}"
        ),
    }
}

/// Polls the relay's metrics until `pred` holds, or fails the test if it never
/// converges within [`STEP_TIMEOUT`]. Guards the async teardown race: an ended
/// session is recorded when the splice task finishes, slightly after the
/// endpoints close.
async fn wait_until_metrics(
    control: &AdmissionControl,
    pred: impl Fn(&MetricsSnapshot) -> bool,
) -> MetricsSnapshot {
    let poll = async {
        loop {
            let snapshot = control.metrics();
            if pred(&snapshot) {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    match timeout(STEP_TIMEOUT, poll).await {
        Ok(snapshot) => snapshot,
        Err(_) => panic!(
            "relay metrics never reached the expected state; last = {:?}",
            control.metrics()
        ),
    }
}

fn expect_consent_identity(event: SessionEvent) -> TargetIdentity {
    match event {
        SessionEvent::ConsentGranted { identity } => identity,
        other => panic!("expected ConsentGranted with honest identity, got {other:?}"),
    }
}

/// Reads agent-side events until the `CommandStarted` for `command` and returns
/// the [`Origin`] the agent saw on the wire (ST-M004-006).
async fn next_command_origin(joiner: &mut JoinHandle, command: &str) -> Origin {
    loop {
        let event = next_event!(joiner);
        if let SessionEvent::CommandStarted {
            command: seen,
            origin,
            ..
        } = event
            && seen == command
        {
            return origin;
        }
    }
}

/// The outcome of draining one job to its terminal event on the console side.
struct ExecResult {
    stdout: Vec<u8>,
    exit_code: Option<i32>,
    outcome: JobOutcome,
}

/// Drains the console's event stream for `job`, collecting stdout until its
/// terminal `ExecFinished`.
async fn collect_exec(host: &mut HostHandle, job: JobId) -> ExecResult {
    let mut stdout = Vec::new();
    loop {
        match next_event!(host) {
            SessionEvent::ExecOutput {
                job: j,
                stream: _,
                bytes,
            } if j == job => stdout.extend_from_slice(&bytes),
            SessionEvent::ExecFinished {
                job: j,
                exit_code,
                outcome,
            } if j == job => {
                return ExecResult {
                    stdout,
                    exit_code,
                    outcome,
                };
            }
            _ => {}
        }
    }
}

/// The observable trust model of one full session over the real relay server.
#[derive(Debug, PartialEq)]
struct TrustObservations {
    sac_matches: bool,
    consent_identity: TargetIdentity,
    command_origin: Origin,
    exit_code: Option<i32>,
    stdout_has_marker: bool,
    revoke_reason: RevokeReason,
}

/// Drives one already-connected pair through the whole attended lifecycle and
/// returns the observable trust tuple. Identical to the direct/over-splice
/// drivers, so a difference here is a difference the real `serve` introduced.
async fn drive_full_lifecycle(
    host: &mut HostHandle,
    joiner: &mut JoinHandle,
    marker: &str,
) -> TrustObservations {
    // Connected → compare gate → AwaitingConsent on both ends. The relay cannot
    // skip the technician's approval-code confirmation (REQ-S-006, ST-M002-013).
    assert_eq!(next_event!(host), SessionEvent::Connected);
    assert_eq!(next_event!(joiner), SessionEvent::Connected);
    assert_eq!(next_event!(host), SessionEvent::AwaitingApprovalCompare);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingApprovalCompare);
    host.confirm_code().expect("confirm the approval code");
    assert_eq!(next_event!(host), SessionEvent::AwaitingConsent);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingConsent);

    // REQ-F-005: both ends independently derive the SAME Session Approval Code.
    // The relay adds no leg to the transcript, so the codes must still match.
    let host_code = host.approval_code();
    let sac_matches = host_code.is_some() && host_code == joiner.approval_code();

    // AC1 / REQ-F-006: a privileged op before consent fails closed, relay or not.
    assert!(
        matches!(
            host.run_command_as(
                ExecRequest::new(Shell::Cmd, "echo before-consent"),
                Origin::Mcp,
            )
            .await,
            Err(SessionError::NotActive)
        ),
        "a privileged op before consent must fail closed, relay or not"
    );

    // Attended consent (REQ-F-006) carrying the honest identity (REQ-F-016).
    joiner.grant_consent().expect("customer grants consent");
    let consent_identity = expect_consent_identity(next_event!(host));
    assert_eq!(next_event!(host), SessionEvent::Active);
    let _ = expect_consent_identity(next_event!(joiner));
    assert_eq!(next_event!(joiner), SessionEvent::Active);

    // MCP authority over the relay (AC2 / ST-M004-006).
    let command = format!("echo {marker}");
    let started = host
        .run_command_as(ExecRequest::new(Shell::Cmd, command.clone()), Origin::Mcp)
        .await
        .expect("run MCP-driven command");
    let command_origin = next_command_origin(joiner, &command).await;
    let result = collect_exec(host, started.job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);
    let stdout_has_marker = String::from_utf8_lossy(&result.stdout).contains(marker);

    // Revocation (REQ-F-018): both ends terminate on the same reason.
    host.revoke(RevokeReason::TechnicianRequested)
        .expect("technician revokes");
    let revoke_reason = drain_until_revoked!(host).expect("host observes revoke");
    let joiner_reason = drain_until_revoked!(joiner).expect("agent observes revoke");
    assert_eq!(
        joiner_reason, revoke_reason,
        "both endpoints see the same revoke reason across the relay server"
    );

    TrustObservations {
        sac_matches,
        consent_identity,
        command_origin,
        exit_code: result.exit_code,
        stdout_has_marker,
        revoke_reason,
    }
}

/// A full attended session pairs, consents, runs an attributed command, and
/// revokes through the **real** relay-server `serve` path, and the server's
/// metadata-only metrics are honest: two legs admitted, two ended, none active,
/// bytes forwarded, nothing denied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_session_survives_the_real_relay_server_and_metrics_are_honest() {
    let (relay_addr, control, _signal, _serve) =
        start_relay(AdmissionPolicy::Open, ServeConfig::default()).await;

    let mut host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps())
        .await
        .expect("bind console");

    // One shared single-use ticket names this pending pairing for both legs.
    let ticket = RendezvousTicket::from_bytes([0x5a; RENDEZVOUS_TICKET_LEN]);
    let console_addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let _console_leg = spawn_console_leg(relay_addr, console_addr, None, ticket.clone());
    let (agent_addr, _agent_leg) = spawn_agent_leg(relay_addr, None, ticket.clone()).await;

    let mut joiner = SessionJoiner::join(agent_addr, agent_identity(), PROTOCOL_V1, caps())
        .await
        .expect("agent dials the relay path");

    let obs = drive_full_lifecycle(&mut host, &mut joiner, "over-real-relay-7c3d").await;

    assert!(
        obs.sac_matches,
        "the Session Approval Code still matches across the real relay server (REQ-F-005)"
    );
    assert_eq!(
        obs.consent_identity,
        agent_identity(),
        "the honest target identity still binds into the grant (REQ-F-016)"
    );
    assert_eq!(
        obs.command_origin,
        Origin::Mcp,
        "MCP authority is preserved through the real relay server (ST-M004-006)"
    );
    assert_eq!(
        obs.exit_code,
        Some(0),
        "the MCP-driven command ran to completion through the relay server"
    );
    assert!(
        obs.stdout_has_marker,
        "the command's output round-tripped end to end through the relay server"
    );
    assert_eq!(
        obs.revoke_reason,
        RevokeReason::TechnicianRequested,
        "revocation is preserved through the relay server (REQ-F-018)"
    );

    // Close the endpoints so both relay legs drain and both guards finish.
    drop(joiner);
    drop(host);

    let m = wait_until_metrics(&control, |m| m.sessions_ended == 2).await;
    assert_eq!(m.sessions_admitted, 2, "both outbound legs were admitted");
    assert_eq!(m.sessions_ended, 2, "both legs ended honestly");
    assert_eq!(m.sessions_active, 0, "no session was left dangling");
    assert!(
        m.bytes_forwarded > 0,
        "the relay forwarded ciphertext (both directions counted once)"
    );
    assert!(m.frames_forwarded > 0, "whole frames were forwarded");
    assert_eq!(m.denied_auth, 0);
    assert_eq!(m.denied_rate_limit, 0);
    assert_eq!(m.denied_quota, 0);
    assert_eq!(m.denied_unavailable, 0);
}

/// A wrong admission credential is denied at authentication before anything is
/// forwarded: the relay says so, the connection is closed with zero bytes moved
/// and the denial is recorded — never a reserved slot (REQ-N-008, ADR-0016 §4,
/// ST-M007-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_admission_token_is_denied_and_forwards_nothing() {
    let policy = AdmissionPolicy::SharedToken {
        token: AdmissionToken::from_text("relay-secret"),
        client: ClientId::new("shared"),
    };
    let (relay_addr, control, _signal, _serve) = start_relay(policy, ServeConfig::default()).await;

    // Dial the relay and present the WRONG secret in the preamble. The relay
    // answers with the reason and closes: nothing is forwarded.
    let (mut relay, answer) = dial_with(relay_addr, Some("not-the-secret"), 0x11).await;
    assert_eq!(
        answer,
        RelayAnswer::Refused(RelayRefusal::CredentialRejected)
    );
    expect_closed(&mut relay).await;

    let m = wait_until_metrics(&control, |m| m.denied_auth == 1).await;
    assert_eq!(m.denied_auth, 1, "the wrong credential was denied at auth");
    assert_eq!(
        m.sessions_admitted, 0,
        "a denied connection never reserves a slot"
    );
    assert_eq!(m.sessions_active, 0);
    assert_eq!(m.bytes_forwarded, 0, "nothing was forwarded");
}

/// A single leg whose rendezvous peer never arrives times out fail-closed: the
/// admitted session is recorded as an honest *unknown* end and its slot is
/// released, never left dangling (REQ-N-008).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lone_arrival_times_out_fail_closed() {
    // A short rendezvous window so the lone leg times out within the test budget.
    let config = ServeConfig {
        rendezvous_timeout: Duration::from_millis(300),
        ..ServeConfig::default()
    };
    let (relay_addr, control, _signal, _serve) = start_relay(AdmissionPolicy::Open, config).await;

    // The lone leg is admitted and told so; then nothing comes.
    let (mut relay, answer) = dial_with(relay_addr, None, 0x22).await;
    assert_eq!(answer, RelayAnswer::Admitted);

    // No peer ever arrives; the parked leg times out, its guard drops, and the
    // relay records an honest unknown end and releases the slot. The endpoint
    // sees the stream end without a byte of a peer: a rendezvous that lapsed,
    // which it can tell from a refusal because it was admitted first.
    expect_closed(&mut relay).await;
    let m = wait_until_metrics(&control, |m| m.sessions_ended == 1).await;
    assert_eq!(m.sessions_admitted, 1, "the lone leg was admitted");
    assert_eq!(
        m.sessions_ended, 1,
        "and ended when its rendezvous peer never came"
    );
    assert_eq!(
        m.sessions_active, 0,
        "no slot was left reserved (fail-closed)"
    );
    assert_eq!(m.bytes_forwarded, 0, "a lone arrival forwards nothing");
    assert_eq!(m.denied_auth, 0, "a lone arrival is admitted, not denied");
}

/// Establishes the direct-LAN trust baseline: the same [`drive_full_lifecycle`]
/// driver over a plain listener/dial pairing, with no relay in the path. The
/// parity test below asserts the relay run reproduces this tuple exactly.
async fn direct_baseline() -> TrustObservations {
    let mut host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps())
        .await
        .expect("bind console (direct baseline)");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let mut joiner = SessionJoiner::join(addr, agent_identity(), PROTOCOL_V1, caps())
        .await
        .expect("agent dials the console directly");
    drive_full_lifecycle(&mut host, &mut joiner, "direct-baseline-1f9e").await
}

/// ST-M007-008 — the **real** endpoint relay dialer pairs a full attended session
/// through the real ST-M007-007 relay server, and the observable trust model is
/// byte-for-byte identical to a direct LAN pairing.
///
/// Unlike the shim-driven cases above, nothing stands in for the endpoints here:
/// the console hosts over an outbound [`RelayDial`] and the agent joins over its
/// own, so the rendezvous preamble is written from *inside* the production
/// [`SessionHost`] / [`SessionJoiner`] runtime — exactly as the real console and
/// agent binaries now do (ADR-0016 §5). Both endpoints dial the relay outbound;
/// the real `serve` matches them on the single-use ticket and splices the two
/// legs, after which the console is the Noise responder and the agent the Noise
/// initiator, precisely as on the LAN. Selecting the relay is a transport choice
/// only, so every downstream session fact — the Session Approval Code, the
/// pre-consent refusal, the honest consent identity, MCP command attribution, and
/// technician revocation — must equal the direct baseline (REQ-F-005,
/// REQ-F-016..018). The relay's metadata-only metrics stay honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_real_endpoint_dialers_pair_through_the_relay_with_direct_path_parity() {
    // The trust model a plain LAN pairing produces, with no relay in the path.
    let baseline = direct_baseline().await;

    let (relay_addr, control, _signal, _serve) =
        start_relay(AdmissionPolicy::Open, ServeConfig::default()).await;

    // One shared single-use ticket names this pending pairing for both legs.
    let ticket = RendezvousTicket::from_bytes([0xa7; RENDEZVOUS_TICKET_LEN]);

    // The console hosts over an outbound relay dial: this parks the first leg at
    // the relay and returns a handle immediately, with the responder handshake
    // running in the background until the agent's leg arrives and is spliced in.
    let mut host = SessionHost::host_dialed(
        RelayDial::new(relay_addr, ticket.clone(), None),
        console_hints(),
        PROTOCOL_V1,
        caps(),
        Arc::new(NoAudit),
    )
    .await
    .expect("console hosts over its own outbound relay dial");

    // The agent joins over its own outbound relay dial; the relay now has both
    // legs on the shared ticket and splices them into one end-to-end session.
    let mut joiner = SessionJoiner::join_with_reboot_resume_over(
        RelayDial::new(relay_addr, ticket.clone(), None),
        agent_identity(),
        PROTOCOL_V1,
        caps(),
        Arc::new(NoAudit),
        Arc::new(|| false),
        Arc::new(NoRebootResume),
    )
    .await
    .expect("agent joins over its own outbound relay dial");

    let over_relay = drive_full_lifecycle(&mut host, &mut joiner, "over-real-dialer-a7f0").await;

    // Parity: every observable trust fact matches the direct baseline. Only the
    // per-run marker text differs, so `stdout_has_marker` is asserted on its own.
    assert_eq!(
        over_relay.sac_matches, baseline.sac_matches,
        "SAC agreement is identical to a direct pairing (REQ-F-005)"
    );
    assert!(
        over_relay.sac_matches,
        "both ends still derive the same Session Approval Code over the relay"
    );
    assert_eq!(
        over_relay.consent_identity, baseline.consent_identity,
        "the honest target identity binds into the grant just as on the LAN (REQ-F-016)"
    );
    assert_eq!(
        over_relay.command_origin, baseline.command_origin,
        "MCP attribution is preserved end to end (ST-M004-006)"
    );
    assert_eq!(
        over_relay.command_origin,
        Origin::Mcp,
        "and the driven command is attributed to the AI, not the operator"
    );
    assert_eq!(
        over_relay.exit_code, baseline.exit_code,
        "the attributed command runs to the same completion over the relay"
    );
    assert!(
        over_relay.stdout_has_marker,
        "the command's output round-tripped end to end through the real dialers"
    );
    assert_eq!(
        over_relay.revoke_reason, baseline.revoke_reason,
        "technician revocation is preserved through the relay (REQ-F-018)"
    );
    assert_eq!(
        over_relay.revoke_reason,
        RevokeReason::TechnicianRequested,
        "and it carries the technician's honest reason",
    );

    // Close the endpoints so both relay legs drain and both guards finish.
    drop(joiner);
    drop(host);

    // AC4: the relay never fabricated or dropped a session — two real legs
    // admitted, two ended, none dangling, ciphertext forwarded, nothing denied.
    let m = wait_until_metrics(&control, |m| m.sessions_ended == 2).await;
    assert_eq!(
        m.sessions_admitted, 2,
        "both real endpoint legs were admitted"
    );
    assert_eq!(m.sessions_ended, 2, "both legs ended honestly");
    assert_eq!(m.sessions_active, 0, "no session was left dangling");
    assert!(
        m.bytes_forwarded > 0,
        "the relay forwarded end-to-end ciphertext for the real session"
    );
    assert!(m.frames_forwarded > 0, "whole frames were forwarded");
    assert_eq!(m.denied_auth, 0);
    assert_eq!(m.denied_rate_limit, 0);
    assert_eq!(m.denied_quota, 0);
    assert_eq!(m.denied_unavailable, 0);
}

/// A relay that asks for a key says that none was presented (ST-M007-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_admission_token_is_answered_as_required() {
    let policy = AdmissionPolicy::SharedToken {
        token: AdmissionToken::from_text("relay-secret"),
        client: ClientId::new("shared"),
    };
    let (relay_addr, control, _signal, _serve) = start_relay(policy, ServeConfig::default()).await;

    let (mut relay, answer) = dial_with(relay_addr, None, 0x31).await;
    assert_eq!(
        answer,
        RelayAnswer::Refused(RelayRefusal::CredentialRequired)
    );
    expect_closed(&mut relay).await;
    let m = wait_until_metrics(&control, |m| m.denied_auth == 1).await;
    assert_eq!(m.sessions_admitted, 0);
}

/// Too many attempts in a short time are answered as such, so that an endpoint
/// waits instead of taking the relay for unreachable (ST-M007-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_attempts_are_answered_as_rate_limited() {
    // One attempt, and no refill within the test.
    let control_config = ControlConfig {
        rate_limit: RateLimit {
            capacity: 1,
            refill_tokens: 1,
            refill_ms: 3_600_000,
            max_clients: 16,
        },
        ..ControlConfig::public_default()
    };
    let (relay_addr, control, _signal, _serve) = start_relay_with(
        AdmissionPolicy::Open,
        control_config,
        ServeConfig::default(),
    )
    .await;

    let (_first, answer) = dial_with(relay_addr, None, 0x41).await;
    assert_eq!(answer, RelayAnswer::Admitted);
    let (mut second, answer) = dial_with(relay_addr, None, 0x42).await;
    assert_eq!(answer, RelayAnswer::Refused(RelayRefusal::RateLimited));
    expect_closed(&mut second).await;
    let m = wait_until_metrics(&control, |m| m.denied_rate_limit == 1).await;
    assert_eq!(
        m.sessions_admitted, 1,
        "only the first attempt was admitted"
    );
}

/// A relay without a free place says so (ST-M007-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_relay_is_answered_as_busy() {
    let control_config = ControlConfig {
        quota: SessionQuota {
            global_max: 1,
            per_client_max: 1,
        },
        ..ControlConfig::public_default()
    };
    let (relay_addr, control, _signal, _serve) = start_relay_with(
        AdmissionPolicy::Open,
        control_config,
        ServeConfig::default(),
    )
    .await;

    // The first leg is admitted and parks: it holds the only place.
    let (_parked, answer) = dial_with(relay_addr, None, 0x51).await;
    assert_eq!(answer, RelayAnswer::Admitted);
    let (mut second, answer) = dial_with(relay_addr, None, 0x52).await;
    assert_eq!(answer, RelayAnswer::Refused(RelayRefusal::Busy));
    expect_closed(&mut second).await;
    let m = wait_until_metrics(&control, |m| m.denied_quota == 1).await;
    assert_eq!(m.sessions_active, 1, "the parked leg keeps its place");
}

/// An endpoint of another preamble version is told which version the relay
/// speaks, instead of a connection that closes without a reason (ST-M007-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_preamble_version_is_answered_with_the_relays_version() {
    use tokio::io::AsyncWriteExt;

    let (relay_addr, control, _signal, _serve) =
        start_relay(AdmissionPolicy::Open, ServeConfig::default()).await;

    // A well-formed preamble of the version before this one.
    let mut relay = TcpStream::connect(relay_addr).await.expect("dial relay");
    let mut preamble = Vec::new();
    preamble.extend_from_slice(&PREAMBLE_MAGIC);
    preamble.push(PREAMBLE_VERSION - 1);
    preamble.extend_from_slice(&0u16.to_le_bytes());
    preamble.extend_from_slice(&[0x61; RENDEZVOUS_TICKET_LEN]);
    relay.write_all(&preamble).await.expect("write preamble");

    assert_eq!(
        answer_of(&mut relay).await,
        RelayAnswer::Refused(RelayRefusal::UnsupportedVersion {
            relay: PREAMBLE_VERSION
        })
    );
    expect_closed(&mut relay).await;
    let m = control.metrics();
    assert_eq!(m.sessions_admitted, 0, "nothing was admitted");
}

/// Something that is no CretLink endpoint gets no answer at all: the relay
/// says nothing to a port scan (ST-M007-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn something_that_is_no_endpoint_gets_no_answer() {
    use tokio::io::AsyncWriteExt;

    let (relay_addr, _control, _signal, _serve) =
        start_relay(AdmissionPolicy::Open, ServeConfig::default()).await;
    let mut other = TcpStream::connect(relay_addr).await.expect("dial relay");
    other
        .write_all(b"GET / HTTP/1.1\r\nHost: relay.example.net\r\n\r\n")
        .await
        .expect("write");
    expect_closed(&mut other).await;
}

/// The host name both copies have in their relay profile in the tests below.
const RELAY_HOST: &str = "relay.example.net";

/// ST-M007-012 — two copies find each other through a typed relay code, also
/// when the customer needs longer than the relay parks a leg: the waiting side
/// dials again every time its rendezvous lapses, and the session that follows is
/// the unchanged attended session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_typed_relay_code_pairs_after_the_waiting_side_outlived_lapsed_rendezvous() {
    // The relay lets a parked leg lapse after a moment, as a real one does
    // after half a minute.
    let config = ServeConfig {
        rendezvous_timeout: Duration::from_millis(150),
        ..ServeConfig::default()
    };
    let (relay_addr, control, _signal, _serve) = start_relay(AdmissionPolicy::Open, config).await;

    // The technician's copy mints a code, shows it, and waits on its ticket.
    let code = RelayCode::generate().expect("mint a relay code");
    let shown = code.encode();
    let waiting_ticket =
        RendezvousTicket::derive(code.seed(), RELAY_HOST).expect("derive the waiting ticket");
    let host_task = tokio::spawn(SessionHost::host_dialed(
        WaitingDial::with_pause(
            RelayDial::new(relay_addr, waiting_ticket, None),
            Duration::from_millis(20),
        ),
        console_hints(),
        PROTOCOL_V1,
        caps(),
        Arc::new(NoAudit),
    ));

    // Nobody joins for a while: several rendezvous lapse and the waiting side
    // dials again each time, without giving up and without a session.
    let m = wait_until_metrics(&control, |m| m.sessions_ended >= 3).await;
    assert!(m.sessions_ended >= 3, "several rendezvous lapsed");
    assert_eq!(m.bytes_forwarded, 0, "nothing was forwarded meanwhile");
    assert!(
        !host_task.is_finished(),
        "the waiting side is still waiting"
    );

    // The customer types the code as people do, and their copy names the same
    // relay in another spelling. Both derive the same ticket.
    let typed = shown.to_lowercase().replace('-', " ");
    let ManualEntry::Relay(typed_code) = parse_entry(&typed).expect("the typed code parses") else {
        panic!("a relay code was typed");
    };
    let joining_ticket = RendezvousTicket::derive(typed_code.seed(), "Relay.Example.NET.")
        .expect("derive the joining ticket");
    let mut joiner = SessionJoiner::join_with_reboot_resume_over(
        RelayDial::new(relay_addr, joining_ticket, None),
        agent_identity(),
        PROTOCOL_V1,
        caps(),
        Arc::new(NoAudit),
        Arc::new(|| false),
        Arc::new(NoRebootResume),
    )
    .await
    .expect("the customer's copy joins through the relay");

    let mut host = timeout(STEP_TIMEOUT, host_task)
        .await
        .expect("the waiting side meets its peer")
        .expect("the waiting task ends in order")
        .expect("the technician's copy hosts over the relay");

    // From here everything is as on a LAN: the approval codes match, consent
    // binds the target, a command runs, and the technician's stop ends it.
    let observed = drive_full_lifecycle(&mut host, &mut joiner, "typed-code-3c1d").await;
    assert!(
        observed.sac_matches,
        "both ends show the same approval code"
    );
    assert_eq!(observed.consent_identity, agent_identity());
    assert_eq!(observed.exit_code, Some(0));
    assert!(observed.stdout_has_marker);
}

/// ST-M007-012 — a code that nobody waits on: the relay admits the leg, lets
/// the rendezvous lapse, and the joining side reports exactly that instead of a
/// pairing failure of unknown cause.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_code_that_nobody_waits_on_is_reported_as_a_lapsed_rendezvous() {
    let config = ServeConfig {
        rendezvous_timeout: Duration::from_millis(150),
        ..ServeConfig::default()
    };
    let (relay_addr, _control, _signal, _serve) = start_relay(AdmissionPolicy::Open, config).await;

    let code = RelayCode::from_seed([0x5a; 8]);
    let ticket = RendezvousTicket::derive(code.seed(), RELAY_HOST).expect("derive");
    let mut joiner = SessionJoiner::join_with_reboot_resume_over(
        RelayDial::new(relay_addr, ticket, None),
        agent_identity(),
        PROTOCOL_V1,
        caps(),
        Arc::new(NoAudit),
        Arc::new(|| false),
        Arc::new(NoRebootResume),
    )
    .await
    .expect("the relay admits the leg");

    match next_event!(joiner) {
        SessionEvent::Failed { stage, message } => {
            assert_eq!(stage, "rendezvous", "{message}");
            assert!(message.contains("nobody joined"), "{message}");
        }
        other => panic!("expected a lapsed rendezvous, got {other:?}"),
    }
}

/// ST-M007-012 — only "nobody joined yet" is waited out. A refusal ends the
/// wait at once with the relay's reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_ends_the_wait_with_the_relays_reason() {
    let policy = AdmissionPolicy::SharedToken {
        token: AdmissionToken::from_text("relay-secret"),
        client: ClientId::new("shared"),
    };
    let (relay_addr, _control, _signal, _serve) = start_relay(policy, ServeConfig::default()).await;
    let ticket = || RendezvousTicket::from_bytes([0x71; RENDEZVOUS_TICKET_LEN]);

    let wrong_key = WaitingDial::new(RelayDial::new(
        relay_addr,
        ticket(),
        Some(AdmissionToken::from_text("not-the-secret")),
    ));
    let error = timeout(STEP_TIMEOUT, wrong_key.dial())
        .await
        .expect("a refusal is not waited out")
        .expect_err("a wrong key is refused");
    assert!(
        matches!(
            error,
            SessionError::Relay(RelayFailure::Refused(RelayRefusal::CredentialRejected))
        ),
        "{error:?}"
    );
    assert!(!rendezvous_lapsed(&error));

    let no_key = WaitingDial::new(RelayDial::new(relay_addr, ticket(), None));
    let error = timeout(STEP_TIMEOUT, no_key.dial())
        .await
        .expect("a refusal is not waited out")
        .expect_err("a missing key is refused");
    assert!(
        matches!(
            error,
            SessionError::Relay(RelayFailure::Refused(RelayRefusal::CredentialRequired))
        ),
        "{error:?}"
    );
}

/// ST-M007-012 — a relay that is not there, and a server that is no relay of
/// this version, are told apart from each other and from a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_relay_and_a_silent_server_are_told_apart() {
    let ticket = || RendezvousTicket::from_bytes([0x72; RENDEZVOUS_TICKET_LEN]);

    // Nothing listens on this address any more.
    let gone = TcpListener::bind(loopback()).await.expect("bind");
    let gone_addr = gone.local_addr().expect("addr");
    drop(gone);
    let error = timeout(
        STEP_TIMEOUT,
        RelayDial::new(gone_addr, ticket(), None).dial(),
    )
    .await
    .expect("a refused connection resolves promptly")
    .expect_err("nothing listens there");
    assert!(
        matches!(error, SessionError::Relay(RelayFailure::Unreachable(_))),
        "{error:?}"
    );

    // A server that takes the connection and closes without an answer, as a
    // relay server of the version before this one does.
    let silent = TcpListener::bind(loopback()).await.expect("bind");
    let silent_addr = silent.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _peer) = silent.accept().await.expect("accept");
        // Read what one preamble holds, then close without a word.
        let mut preamble = [0u8; 4 + 1 + 2 + RENDEZVOUS_TICKET_LEN];
        let _ = stream.read_exact(&mut preamble).await;
    });
    let error = timeout(
        STEP_TIMEOUT,
        RelayDial::new(silent_addr, ticket(), None).dial(),
    )
    .await
    .expect("a closed connection resolves promptly")
    .expect_err("no answer came");
    assert!(
        matches!(error, SessionError::Relay(RelayFailure::NoAnswer(_))),
        "{error:?}"
    );
    server.await.expect("the silent server ends");
}
