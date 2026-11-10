//! ST-M007-009 — the relay-server's **wss-over-TLS** transport envelope carries a
//! real end-to-end CretLink session with direct-path parity, and every
//! authentication fault fails the dial closed.
//!
//! Where [`relay_server.rs`] proves the raw-TCP `serve` composition is an opaque
//! forwarder, this suite proves the same across the **shipped default** transport
//! (ADR-0016 §3): the real [`serve_wss`] path TLS-terminates each accepted
//! connection and upgrades it to a WebSocket before the *identical* preamble →
//! admit → rendezvous → splice pipeline runs. Both endpoints dial through their
//! production [`WssRelayDial`], so the rendezvous preamble and the whole Noise
//! session now ride inside authenticated TLS records — yet the relay still holds
//! no session key and decodes no protocol message.
//!
//! The certificate and key are minted at runtime with the pure-Rust
//! [`cretlink_wss::testcert::self_signed`] helper (feature `test-cert`); no key
//! material is committed (REQ-N-017), and the same self-signed leaf the relay
//! presents is pinned by the client connector to complete the trust path — the
//! self-hosted-relay trust model exactly (ADR-0016 §3, ST-M007-005).
//!
//! The suite asserts four things about the real wss server:
//!
//! 1. **Parity** — a full attended lifecycle (SAC match, pre-consent refusal,
//!    honest consent identity, an `Origin::Mcp` command, technician revocation)
//!    over the wss path is byte-for-byte identical to a direct LAN pairing, and
//!    the metadata-only metrics are honest (REQ-F-005, REQ-F-016..018).
//! 2. **Untrusted certificate fails closed** — a connector pinning a *different*
//!    self-signed leaf aborts the TLS handshake; no stream is returned and the
//!    rendezvous preamble never leaves the endpoint (REQ-S-020, REQ-N-015).
//! 3. **Hostname mismatch fails closed** — pinning the real leaf but validating a
//!    server name it was not issued for likewise aborts the handshake.
//! 4. **A plaintext dial is refused** — a raw TCP connection that writes a
//!    cleartext preamble to the wss port is dropped at the TLS accept, admitting
//!    no session and forwarding nothing (REQ-N-008).
//!
//! [`relay_server.rs`]: ./relay_server.rs

// This test binary follows the crate e2e style: `unwrap`/`expect`/`panic` are the
// assertion vocabulary, allowed so shared driver/helper code is covered too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use cretlink_protocol::{Origin, PROTOCOL_V1, TargetIdentity};
use cretlink_relay::{
    AdmissionControl, AdmissionPolicy, ControlConfig, MetricsSnapshot, Preamble,
    RENDEZVOUS_TICKET_LEN, Rendezvous, RendezvousTicket, StopSignal, write_preamble,
};
use cretlink_relay_server::{Leg, ServeConfig, serve_wss};
use cretlink_session::{
    ConsoleHints, Dial, ExecRequest, HostHandle, JobId, JobOutcome, JoinHandle, NoAudit,
    NoRebootResume, RevokeReason, SessionError, SessionEvent, SessionHost, SessionJoiner, Shell,
    WssRelayDial,
};
use cretlink_wss::testcert::self_signed;
use cretlink_wss::{CertificateDer, PrivateKeyDer, WssConnector, WssServerStream};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// The DNS name the runtime-minted relay leaf is issued for; the client connector
/// pins this same leaf and validates it against this name.
const RELAY_DNS: &str = "relay.test";

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

/// Stands up the **real** relay server over `serve_wss` on a loopback port,
/// TLS-terminating each connection with `cert`/`key`. Returns its address, a
/// handle to read its live metrics, the [`StopSignal`] that must be kept alive
/// (dropping it fails the server closed), and the serve task.
async fn start_relay_wss(
    policy: AdmissionPolicy,
    config: ServeConfig,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> (
    SocketAddr,
    AdmissionControl,
    StopSignal,
    tokio::task::JoinHandle<()>,
) {
    let control = AdmissionControl::new(policy, ControlConfig::public_default());
    let rendezvous: Arc<Rendezvous<Leg<WssServerStream<TcpStream>>>> = Arc::new(Rendezvous::new());
    let server_config = cretlink_wss::server_config(vec![cert], key).expect("build server config");
    let acceptor = cretlink_wss::acceptor(server_config);
    let listener = TcpListener::bind(loopback()).await.expect("bind relay");
    let addr = listener.local_addr().expect("relay addr");
    let (signal, stop) = StopSignal::new();
    let task = tokio::spawn(serve_wss(
        listener,
        acceptor,
        control.clone(),
        rendezvous,
        config,
        stop,
    ));
    (addr, control, signal, task)
}

/// Builds a client connector that trusts **exactly** the pinned leaf `cert` and
/// nothing else — the self-hosted-relay trust model (the operator hands its
/// certificate to the endpoints to pin, ADR-0016 §3).
fn pinned_connector(cert: CertificateDer<'static>) -> WssConnector {
    let roots = cretlink_wss::pinned_roots(vec![cert]).expect("pin the relay leaf");
    let config = cretlink_wss::client_config(roots).expect("build client config");
    cretlink_wss::connector(config)
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
/// returns the observable trust tuple. Identical to the direct/raw-relay drivers,
/// so a difference here is a difference the wss `serve_wss` path introduced.
async fn drive_full_lifecycle(
    host: &mut HostHandle,
    joiner: &mut JoinHandle,
    marker: &str,
) -> TrustObservations {
    // Connected → compare gate → AwaitingConsent on both ends. The wss relay
    // cannot skip the technician's approval-code confirmation (REQ-S-006,
    // ST-M002-013).
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
        "a privileged op before consent must fail closed, wss or not"
    );

    // Attended consent (REQ-F-006) carrying the honest identity (REQ-F-016).
    joiner.grant_consent().expect("customer grants consent");
    let consent_identity = expect_consent_identity(next_event!(host));
    assert_eq!(next_event!(host), SessionEvent::Active);
    let _ = expect_consent_identity(next_event!(joiner));
    assert_eq!(next_event!(joiner), SessionEvent::Active);

    // MCP authority over the wss relay (AC2 / ST-M004-006).
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
        "both endpoints see the same revoke reason across the wss relay server"
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

/// Establishes the direct-LAN trust baseline: the same [`drive_full_lifecycle`]
/// driver over a plain listener/dial pairing, with no relay in the path. The
/// parity test below asserts the wss run reproduces this tuple exactly.
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
    drive_full_lifecycle(&mut host, &mut joiner, "direct-baseline-9d2b").await
}

/// AC / ST-M007-009 — the **real** endpoint wss dialer pairs a full attended
/// session through the real relay server over TLS + WebSocket, and the observable
/// trust model is byte-for-byte identical to a direct LAN pairing.
///
/// Nothing stands in for the endpoints: the console hosts over an outbound
/// [`WssRelayDial`] and the agent joins over its own, so the rendezvous preamble
/// is written from *inside* the production runtime — over an authenticated TLS
/// channel whose certificate each endpoint validates against the pinned leaf.
/// Selecting wss is a transport choice only, so every downstream session fact —
/// the Session Approval Code, the pre-consent refusal, the honest consent
/// identity, MCP command attribution, and technician revocation — must equal the
/// direct baseline (REQ-F-005, REQ-F-016..018). The relay's metadata-only metrics
/// stay honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_real_endpoint_dialers_pair_through_a_wss_relay_with_direct_path_parity() {
    // The trust model a plain LAN pairing produces, with no relay in the path.
    let baseline = direct_baseline().await;

    // Mint the relay's leaf once; the client connector pins the same certificate.
    let (cert, key) = self_signed(RELAY_DNS).expect("mint a self-signed relay leaf");
    let connector = pinned_connector(cert.clone());
    let (relay_addr, control, _signal, _serve) =
        start_relay_wss(AdmissionPolicy::Open, ServeConfig::default(), cert, key).await;

    // One shared single-use ticket names this pending pairing for both legs.
    let ticket = RendezvousTicket::from_bytes([0xc4; RENDEZVOUS_TICKET_LEN]);

    // The console hosts over an outbound wss relay dial: TLS + WebSocket complete,
    // the preamble parks the first leg at the relay, and the responder handshake
    // runs in the background until the agent's leg arrives and is spliced in.
    let mut host = SessionHost::host_dialed(
        WssRelayDial::new(
            relay_addr,
            RELAY_DNS,
            connector.clone(),
            ticket.clone(),
            None,
        ),
        console_hints(),
        PROTOCOL_V1,
        caps(),
        Arc::new(NoAudit),
    )
    .await
    .expect("console hosts over its own outbound wss relay dial");

    // The agent joins over its own outbound wss relay dial; the relay now has both
    // legs on the shared ticket and splices them into one end-to-end session.
    let mut joiner = SessionJoiner::join_with_reboot_resume_over(
        WssRelayDial::new(
            relay_addr,
            RELAY_DNS,
            connector.clone(),
            ticket.clone(),
            None,
        ),
        agent_identity(),
        PROTOCOL_V1,
        caps(),
        Arc::new(NoAudit),
        Arc::new(|| false),
        Arc::new(NoRebootResume),
    )
    .await
    .expect("agent joins over its own outbound wss relay dial");

    let over_wss = drive_full_lifecycle(&mut host, &mut joiner, "over-wss-relay-c4f1").await;

    // Parity: every observable trust fact matches the direct baseline. Only the
    // per-run marker text differs, so `stdout_has_marker` is asserted on its own.
    assert_eq!(
        over_wss.sac_matches, baseline.sac_matches,
        "SAC agreement is identical to a direct pairing (REQ-F-005)"
    );
    assert!(
        over_wss.sac_matches,
        "both ends still derive the same Session Approval Code over wss"
    );
    assert_eq!(
        over_wss.consent_identity, baseline.consent_identity,
        "the honest target identity binds into the grant just as on the LAN (REQ-F-016)"
    );
    assert_eq!(
        over_wss.command_origin, baseline.command_origin,
        "MCP attribution is preserved end to end (ST-M004-006)"
    );
    assert_eq!(
        over_wss.command_origin,
        Origin::Mcp,
        "and the driven command is attributed to the AI, not the operator"
    );
    assert_eq!(
        over_wss.exit_code, baseline.exit_code,
        "the attributed command runs to the same completion over wss"
    );
    assert!(
        over_wss.stdout_has_marker,
        "the command's output round-tripped end to end through the wss dialers"
    );
    assert_eq!(
        over_wss.revoke_reason, baseline.revoke_reason,
        "technician revocation is preserved through the wss relay (REQ-F-018)"
    );
    assert_eq!(
        over_wss.revoke_reason,
        RevokeReason::TechnicianRequested,
        "and it carries the technician's honest reason",
    );

    // Close the endpoints so both relay legs drain and both guards finish.
    drop(joiner);
    drop(host);

    // The relay never fabricated or dropped a session — two real legs admitted,
    // two ended, none dangling, ciphertext forwarded inside TLS, nothing denied.
    let m = wait_until_metrics(&control, |m| m.sessions_ended == 2).await;
    assert_eq!(m.sessions_admitted, 2, "both real wss legs were admitted");
    assert_eq!(m.sessions_ended, 2, "both legs ended honestly");
    assert_eq!(m.sessions_active, 0, "no session was left dangling");
    assert!(
        m.bytes_forwarded > 0,
        "the relay forwarded end-to-end ciphertext for the real wss session"
    );
    assert!(m.frames_forwarded > 0, "whole frames were forwarded");
    assert_eq!(m.denied_auth, 0);
    assert_eq!(m.denied_rate_limit, 0);
    assert_eq!(m.denied_quota, 0);
    assert_eq!(m.denied_unavailable, 0);
}

/// AC / ST-M007-009 — an endpoint whose connector pins a **different**
/// certificate than the relay presents fails the TLS handshake closed: `dial`
/// returns no stream, so the rendezvous preamble (and thus the pairing metadata)
/// never leaves the endpoint (REQ-S-020, REQ-N-015).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_untrusted_relay_certificate_fails_the_wss_dial_closed() {
    // The relay presents leaf A; the endpoint pins leaf B (same DNS name, distinct
    // key) — a valid-looking certificate for the right host that is simply not the
    // one the relay serves.
    let (cert_a, key_a) = self_signed(RELAY_DNS).expect("mint relay leaf A");
    let (cert_b, _key_b) = self_signed(RELAY_DNS).expect("mint an unrelated leaf B");
    let untrusted = pinned_connector(cert_b);
    let (relay_addr, _control, _signal, _serve) =
        start_relay_wss(AdmissionPolicy::Open, ServeConfig::default(), cert_a, key_a).await;

    let ticket = RendezvousTicket::from_bytes([0x31; RENDEZVOUS_TICKET_LEN]);
    let dialer = WssRelayDial::new(relay_addr, RELAY_DNS, untrusted, ticket, None);

    let result = timeout(STEP_TIMEOUT, dialer.dial())
        .await
        .expect("the untrusted handshake resolves promptly");
    assert!(
        matches!(result, Err(SessionError::Io(_))),
        "an untrusted relay certificate must fail the dial closed with no stream"
    );
}

/// AC / ST-M007-009 — pinning the relay's real leaf but validating a **server
/// name it was not issued for** likewise aborts the TLS handshake: the leaf's
/// only SAN is `relay.test`, so a dial for a different name fails closed
/// (REQ-S-020).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostname_mismatch_fails_the_wss_dial_closed() {
    let (cert, key) = self_signed(RELAY_DNS).expect("mint the relay leaf");
    let trusted = pinned_connector(cert.clone());
    let (relay_addr, _control, _signal, _serve) =
        start_relay_wss(AdmissionPolicy::Open, ServeConfig::default(), cert, key).await;

    let ticket = RendezvousTicket::from_bytes([0x42; RENDEZVOUS_TICKET_LEN]);
    // Trust anchor is correct, but the validated name is not on the certificate.
    let dialer = WssRelayDial::new(relay_addr, "not-the-relay.test", trusted, ticket, None);

    let result = timeout(STEP_TIMEOUT, dialer.dial())
        .await
        .expect("the mismatched handshake resolves promptly");
    assert!(
        matches!(result, Err(SessionError::Io(_))),
        "a hostname mismatch must fail the dial closed with no stream"
    );
}

/// AC / ST-M007-009 — a raw TCP client that writes a **cleartext** rendezvous
/// preamble to the wss port is dropped at the TLS accept: the bytes are read as a
/// malformed ClientHello, the connection is closed with nothing forwarded, and no
/// session is ever admitted (REQ-N-008).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plaintext_dial_to_the_wss_relay_is_refused() {
    let (cert, key) = self_signed(RELAY_DNS).expect("mint the relay leaf");
    let (relay_addr, control, _signal, _serve) =
        start_relay_wss(AdmissionPolicy::Open, ServeConfig::default(), cert, key).await;

    // Dial the wss port with a *plain* TCP socket and write a cleartext preamble,
    // exactly as a raw-`serve` client would. The TLS acceptor reads it as a
    // malformed ClientHello and rejects it.
    let mut relay = TcpStream::connect(relay_addr).await.expect("dial relay");
    let ticket = RendezvousTicket::from_bytes([0x53; RENDEZVOUS_TICKET_LEN]);
    // The write may buffer before the server tears the connection down; either way
    // the session cannot proceed, so its result is not the assertion — the fact
    // that the connection terminates, and the relay's metrics, are.
    let _ = write_preamble(&mut relay, &Preamble::new(None, ticket)).await;

    // The relay never turns this into a forwarding channel: the TLS layer aborts
    // the handshake (emitting at most a short failure alert) and closes, so the
    // read side must reach a terminal state — EOF or a reset — promptly. Any bytes
    // drained here are that TLS alert, never forwarded session data; the relay
    // metrics below are the authoritative "nothing was forwarded" proof.
    let mut alert = Vec::new();
    match timeout(STEP_TIMEOUT, relay.read_to_end(&mut alert)).await {
        Ok(Ok(_)) => {}  // reached EOF: the relay refused and closed the connection.
        Ok(Err(_)) => {} // reset mid-handshake: also fail-closed.
        Err(_) => panic!("the wss relay did not promptly refuse a plaintext dial"),
    }

    // The upgrade failed before admission, so no slot was ever reserved and no
    // ciphertext moved: the plaintext dial is invisible to the rendezvous stage.
    let m = control.metrics();
    assert_eq!(
        m.sessions_admitted, 0,
        "a plaintext dial is dropped at TLS accept, never admitted"
    );
    assert_eq!(m.sessions_active, 0, "no slot was reserved");
    assert_eq!(m.bytes_forwarded, 0, "nothing was forwarded");
}
