//! ST-M007-003 — the full attended-session trust model survives the optional
//! internet relay splice **unchanged and identically to the direct LAN path**.
//!
//! The production session runtime speaks TCP end to end
//! ([`SessionHost::host`]/[`SessionJoiner::join`]). To interpose a relay without
//! touching a line of that runtime, we splice at the TCP layer: the agent dials
//! a local relay port, the relay accepts that leg and `connect`s the real
//! console leg, and [`cretlink_relay::splice`] moves the length-delimited Noise
//! frames between them one whole frame at a time per direction (ST-M007-002).
//! Neither endpoint knows the relay is there — exactly the internet-path
//! deployment (ADR-0016) — so any behaviour that matches the direct path
//! proves the relay changed nothing.
//!
//! The tests assert the whole trust tuple is identical over the relay and
//! direct: the Session Approval Code still matches on both ends (REQ-F-005),
//! attended consent still gates every privileged operation (REQ-F-006), the
//! honest target identity still binds into the grant (REQ-F-016), revocation is
//! preserved (REQ-F-018), and an AI-driven command still reaches the agent with
//! `Origin::Mcp` authority (ST-M004-006) — while the relay observes only opaque
//! frames (REQ-S-005). The adversarial modify/replay/reorder relay is the
//! separate ST-M007-006 model; here the relay is honest and only its *presence*
//! is under test.
//!

// This test binary follows the crate's e2e style: `unwrap`/`expect`/`panic` are
// the assertion vocabulary, allowed crate-wide so shared driver/helper code is
// covered too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use cretlink_protocol::{Origin, PROTOCOL_V1, TargetIdentity};
use cretlink_relay::{SpliceLimits, SpliceReport, StopSignal, splice};
use cretlink_session::{
    ConsoleHints, ExecRequest, HostHandle, JobId, JobOutcome, JoinHandle, RevokeReason,
    SessionError, SessionEvent, SessionHost, SessionJoiner, Shell,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// Per-event wait budget: localhost pairing/consent/exec is sub-millisecond, so
/// this only guards against a hang.
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// The tokio task that runs one relay splice; its output is the metadata-only
/// [`SpliceReport`].
type RelayTask = tokio::task::JoinHandle<SpliceReport>;

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
/// returning the reason. Tolerant of any events that precede it, so a racing
/// teardown never desynchronises the assertion.
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

/// The shared capability set both ends hash into the Noise prologue; a mismatch
/// fails the handshake by design.
fn caps() -> Vec<String> {
    vec!["exec".to_string(), "files".to_string()]
}

/// The observable trust model of one full session, captured so a relay run and a
/// direct run can be compared for exact equality. Only a per-run echo marker
/// differs between runs, and that is reduced to the `stdout_has_marker` bool so
/// the tuples stay comparable.
#[derive(Debug, PartialEq)]
struct TrustObservations {
    /// Both endpoints independently derived the same Session Approval Code
    /// (REQ-F-005).
    sac_matches: bool,
    /// The honest target identity carried in the consent grant (REQ-F-016).
    consent_identity: TargetIdentity,
    /// The authority the agent saw on the AI-driven command (ST-M004-006).
    command_origin: Origin,
    /// The command's exit code.
    exit_code: Option<i32>,
    /// Whether the command's output round-tripped end to end.
    stdout_has_marker: bool,
    /// The reason both endpoints terminated on (REQ-F-018).
    revoke_reason: RevokeReason,
}

/// Binds a console on loopback and dials it with an agent **directly**, the
/// baseline LAN path.
async fn direct_pair() -> (HostHandle, JoinHandle) {
    let host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps())
        .await
        .expect("bind console");
    let joiner = SessionJoiner::join(
        host.local_addr()
            .expect("a LAN-bound console reports its local listener address"),
        agent_identity(),
        PROTOCOL_V1,
        caps(),
    )
    .await
    .expect("dial agent directly");
    (host, joiner)
}

/// Stands up a TCP relay in front of `host_addr`: it accepts the agent's dial,
/// connects the real console leg, and splices the two, forwarding opaque frames.
/// The returned [`StopSignal`] must be kept alive — dropping it fails the splice
/// closed.
async fn relay_interposer(host_addr: SocketAddr) -> (SocketAddr, StopSignal, RelayTask) {
    let listener = TcpListener::bind(loopback()).await.expect("bind relay");
    let relay_addr = listener.local_addr().expect("relay addr");
    let (signal, stop) = StopSignal::new();
    let task = tokio::spawn(async move {
        let (agent_leg, _peer) = listener.accept().await.expect("relay accepts agent leg");
        let console_leg = TcpStream::connect(host_addr)
            .await
            .expect("relay dials console leg");
        splice(agent_leg, console_leg, SpliceLimits::default(), stop).await
    });
    (relay_addr, signal, task)
}

/// Like [`relay_interposer`], but records every byte the relay reads on both
/// legs — i.e. everything it forwards, in both directions — into `log`.
async fn recording_relay_interposer(
    host_addr: SocketAddr,
    log: Arc<Mutex<Vec<u8>>>,
) -> (SocketAddr, StopSignal, RelayTask) {
    let listener = TcpListener::bind(loopback()).await.expect("bind relay");
    let relay_addr = listener.local_addr().expect("relay addr");
    let (signal, stop) = StopSignal::new();
    let task = tokio::spawn(async move {
        let (agent_leg, _peer) = listener.accept().await.expect("relay accepts agent leg");
        let console_leg = TcpStream::connect(host_addr)
            .await
            .expect("relay dials console leg");
        let agent_leg = Recorder::new(agent_leg, Arc::clone(&log));
        let console_leg = Recorder::new(console_leg, log);
        splice(agent_leg, console_leg, SpliceLimits::default(), stop).await
    });
    (relay_addr, signal, task)
}

fn expect_consent_identity(event: SessionEvent) -> TargetIdentity {
    match event {
        SessionEvent::ConsentGranted { identity } => identity,
        other => panic!("expected ConsentGranted with honest identity, got {other:?}"),
    }
}

/// Reads agent-side events until the `CommandStarted` for `command` and returns
/// the [`Origin`] the agent saw on the wire — the attribution the customer-facing
/// surface renders (ST-M004-006).
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

/// Drives one already-connected pair through the whole attended lifecycle —
/// pairing SAC, the pre-consent privileged-op refusal, attended consent with
/// identity, an MCP-authority command, and technician revocation — and returns
/// the observable trust tuple. Identical for the relay and direct runs, so a
/// difference in the tuple is a difference the relay introduced.
async fn drive_full_lifecycle(
    host: &mut HostHandle,
    joiner: &mut JoinHandle,
    marker: &str,
) -> TrustObservations {
    // Connected → compare gate → AwaitingConsent on both ends. The compare gate
    // (REQ-S-006, ST-M002-013) pauses both ends until the technician confirms
    // the codes match; the relay cannot skip it any more than it can skip
    // consent.
    assert_eq!(next_event!(host), SessionEvent::Connected);
    assert_eq!(next_event!(joiner), SessionEvent::Connected);
    assert_eq!(next_event!(host), SessionEvent::AwaitingApprovalCompare);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingApprovalCompare);
    host.confirm_code().expect("confirm the approval code");
    assert_eq!(next_event!(host), SessionEvent::AwaitingConsent);
    assert_eq!(next_event!(joiner), SessionEvent::AwaitingConsent);

    // REQ-F-005: both ends independently derive the SAME Session Approval Code
    // once the handshake completes. The relay adds no leg of its own to the
    // transcript, so the codes must still match — a relay-in-the-middle would
    // show mismatched codes and the customer would abort.
    let host_code = host.approval_code();
    let sac_matches = host_code.is_some() && host_code == joiner.approval_code();

    // AC1 / REQ-F-006: relay presence cannot skip or weaken consent. Before the
    // customer grants control, no privileged operation runs — not even one the
    // AI drives. The gate is local to the endpoint, so the relay cannot bypass
    // it; the same fail-closed refusal must hold over the relay and direct.
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

    // Attended consent (REQ-F-006) carrying the honest target identity
    // (REQ-F-016); both ends reach Active.
    joiner.grant_consent().expect("customer grants consent");
    let consent_identity = expect_consent_identity(next_event!(host));
    assert_eq!(next_event!(host), SessionEvent::Active);
    let _ = expect_consent_identity(next_event!(joiner));
    assert_eq!(next_event!(joiner), SessionEvent::Active);

    // MCP authority over the relay (AC2 / ST-M004-006): the AI-driven command is
    // stamped `Origin::Mcp` on the wire and must reach the agent with that exact
    // attribution, identically to the LAN path.
    let command = format!("echo {marker}");
    let started = host
        .run_command_as(ExecRequest::new(Shell::Cmd, command.clone()), Origin::Mcp)
        .await
        .expect("run MCP-driven command");
    let command_origin = next_command_origin(joiner, &command).await;
    let result = collect_exec(host, started.job).await;
    assert_eq!(result.outcome, JobOutcome::Completed);
    let stdout_has_marker = String::from_utf8_lossy(&result.stdout).contains(marker);

    // Revocation (REQ-F-018): the technician revokes; both ends terminate on the
    // same reason, fail-closed.
    host.revoke(RevokeReason::TechnicianRequested)
        .expect("technician revokes");
    let revoke_reason = drain_until_revoked!(host).expect("host observes revoke");
    let joiner_reason = drain_until_revoked!(joiner).expect("agent observes revoke");
    assert_eq!(
        joiner_reason, revoke_reason,
        "both endpoints see the same revoke reason across the relay"
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

/// The whole observable trust model — pairing SAC, consent, target identity,
/// MCP authority, exec outcome, and revocation — is byte-for-byte identical
/// whether the session runs directly over the LAN or through the opaque relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_trust_model_is_identical_over_the_relay_and_the_direct_path() {
    // Baseline: the direct LAN path.
    let (mut host_d, mut joiner_d) = direct_pair().await;
    let direct = drive_full_lifecycle(&mut host_d, &mut joiner_d, "direct-lan-9c1f").await;

    // The same driver, with the opaque relay spliced in at the TCP layer.
    let mut host_r = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps())
        .await
        .expect("bind console");
    let (relay_addr, _signal, _relay) = relay_interposer(
        host_r
            .local_addr()
            .expect("a LAN-bound console reports its local listener address"),
    )
    .await;
    let mut joiner_r = SessionJoiner::join(relay_addr, agent_identity(), PROTOCOL_V1, caps())
        .await
        .expect("agent dials the relay");
    let over_relay = drive_full_lifecycle(&mut host_r, &mut joiner_r, "over-relay-4b7e").await;

    // The entire trust tuple is identical; only the per-run echo marker differed,
    // and the driver records only whether it round-tripped.
    assert_eq!(
        over_relay, direct,
        "the relay must preserve the LAN pairing/consent/identity/authority model exactly"
    );
    // Spell out each guarantee so a regression names itself.
    assert!(
        over_relay.sac_matches,
        "the Session Approval Code still matches on both ends across the relay (REQ-F-005)"
    );
    assert_eq!(
        over_relay.consent_identity,
        agent_identity(),
        "the honest target identity still binds into the grant (REQ-F-016)"
    );
    assert_eq!(
        over_relay.command_origin,
        Origin::Mcp,
        "relay-to-LAN transport changes do not alter MCP authority (AC2)"
    );
    assert_eq!(
        over_relay.exit_code,
        Some(0),
        "the MCP-driven command ran to completion over the relay"
    );
    assert!(
        over_relay.stdout_has_marker,
        "the command's output round-tripped end to end over the relay"
    );
    assert_eq!(
        over_relay.revoke_reason,
        RevokeReason::TechnicianRequested,
        "revocation is preserved over the relay (REQ-F-018)"
    );
}

/// The relay forwards only opaque frames for the **whole** consented session,
/// not just the handshake: a marker that crosses end to end in cleartext at the
/// endpoints — inside the MCP command and its echoed output — never appears in
/// the bytes the relay forwards (REQ-S-005).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_relay_forwards_only_opaque_frames_for_the_whole_session() {
    const MARKER: &str = "CRETLINK-RELAY-SESSION-MARKER-9f2a1c";

    let mut host = SessionHost::host(loopback(), console_hints(), PROTOCOL_V1, caps())
        .await
        .expect("bind console");
    let log = Arc::new(Mutex::new(Vec::new()));
    let (relay_addr, _signal, relay) = recording_relay_interposer(
        host.local_addr()
            .expect("a LAN-bound console reports its local listener address"),
        Arc::clone(&log),
    )
    .await;
    let mut joiner = SessionJoiner::join(relay_addr, agent_identity(), PROTOCOL_V1, caps())
        .await
        .expect("agent dials the relay");

    let obs = drive_full_lifecycle(&mut host, &mut joiner, MARKER).await;
    assert_eq!(obs.command_origin, Origin::Mcp);
    assert!(
        obs.stdout_has_marker,
        "the marker crosses end to end in cleartext at the endpoints"
    );

    // Close both session legs so the splice drains and the log is complete.
    drop(joiner);
    drop(host);
    let report = timeout(STEP_TIMEOUT, relay)
        .await
        .expect("relay task joins in time")
        .expect("relay task did not panic");
    assert!(
        report.a_to_b_frames > 0 && report.b_to_a_frames > 0,
        "the relay forwarded whole frames in both directions"
    );

    let forwarded = log.lock().expect("log lock").clone();
    assert!(!forwarded.is_empty(), "the relay did forward bytes");
    assert!(
        !contains_subsequence(&forwarded, MARKER.as_bytes()),
        "the MCP command text and its echo crossed the relay only as ciphertext (REQ-S-005)"
    );
}

/// True if `needle` occurs as a contiguous subsequence of `haystack`.
fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A pass-through stream that appends every byte it *reads* to a shared log —
/// used to capture exactly what the relay forwards.
struct Recorder<S> {
    inner: S,
    log: Arc<Mutex<Vec<u8>>>,
}

impl<S> Recorder<S> {
    fn new(inner: S, log: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { inner, log }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Recorder<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &res {
            let fresh = &buf.filled()[before..];
            if !fresh.is_empty()
                && let Ok(mut log) = self.log.lock()
            {
                log.extend_from_slice(fresh);
            }
        }
        res
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Recorder<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
