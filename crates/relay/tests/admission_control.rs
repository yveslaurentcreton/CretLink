//! End-to-end proof that the relay's admission-control gate composes with the
//! opaque splice (ST-M007-004, ADR-0016 §4–§5).
//!
//! The unit tests in `control.rs` exercise the gate against synthetic reports.
//! This test closes the operational seam: a connection is admitted *before* any
//! byte moves, its [`splice_limits`](cretlink_relay::SessionGuard::splice_limits)
//! bound a *real* Noise `XX` session forwarded across the splice, and finishing
//! the guard folds the resulting [`SpliceReport`](cretlink_relay::SpliceReport)
//! into honest, metadata-only relay metrics and an operational event trail —
//! never any session secret or plaintext.
//!

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cretlink_protocol::PROTOCOL_V1;
use cretlink_relay::{
    AdmissionControl, AdmissionPolicy, ControlConfig, OpEventKind, StopSignal, splice,
};
use cretlink_transport::{Pairing, Role};
use tokio::io::duplex;

/// Both `Pairing::perform` calls must hash the same capability set into the
/// prologue or the handshake fails by design; this is that shared set.
fn caps() -> Vec<String> {
    vec!["exec".to_string()]
}

/// An admitted session forwards a real end-to-end Noise pairing through the
/// splice, and finishing its guard reports honest totals through the gate while
/// releasing the concurrency slot.
#[tokio::test]
async fn an_admitted_session_forwards_and_reports_through_the_gate() {
    // A self-hosted, open relay still meters every session for abuse control.
    let control = AdmissionControl::new(AdmissionPolicy::Open, ControlConfig::default());

    // The connection is admitted before any byte is forwarded; the gate hands
    // back the per-session forwarding bounds and marks the session active.
    let guard = control
        .admit(None, "203.0.113.7:44321", 0)
        .expect("an open relay admits the peer");
    assert_eq!(control.metrics().sessions_active, 1);
    let limits = guard.splice_limits();

    // Drive the real Noise XX pairing across the splice under those bounds.
    let (agent_end, relay_a) = duplex(4096);
    let (relay_b, console_end) = duplex(4096);
    let (_signal, stop) = StopSignal::new();
    let relay = tokio::spawn(splice(relay_a, relay_b, limits, stop));

    let shared = caps();
    let (agent, console) = tokio::join!(
        Pairing::perform(agent_end, Role::Initiator, PROTOCOL_V1, &shared),
        Pairing::perform(console_end, Role::Responder, PROTOCOL_V1, &shared),
    );
    let agent = agent.expect("agent pairs through the gated relay");
    let console = console.expect("console pairs through the gated relay");
    assert_eq!(
        agent.approval_code, console.approval_code,
        "the gate forwards ciphertext only: both endpoints derive the same SAC"
    );

    // Dropping the paired endpoints closes both relay legs; the splice ends and
    // yields its metadata-only report.
    drop(agent);
    drop(console);
    let report = relay.await.expect("relay task joins");
    assert!(
        report.a_to_b_frames > 0 && report.b_to_a_frames > 0,
        "real frames crossed the splice in both directions"
    );
    let expected_bytes = report.a_to_b_bytes.saturating_add(report.b_to_a_bytes);
    let expected_frames = report.a_to_b_frames.saturating_add(report.b_to_a_frames);

    // Finishing the guard releases the slot and folds the honest totals into the
    // gate's metrics.
    guard.finish(1, &report);

    let m = control.metrics();
    assert_eq!(m.sessions_admitted, 1);
    assert_eq!(m.sessions_active, 0, "the slot was released on finish");
    assert_eq!(m.sessions_ended, 1);
    assert_eq!(m.bytes_forwarded, expected_bytes);
    assert_eq!(m.frames_forwarded, expected_frames);

    // The gate recorded an admit and a session-end event — metadata only, keyed
    // to the peer label, carrying no secret or session content.
    let events = control.recent_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, OpEventKind::Admitted)),
        "an admit event was recorded"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, OpEventKind::SessionEnded { .. })),
        "a session-end event was recorded"
    );
    assert!(
        events.iter().all(|e| e.peer == "203.0.113.7:44321"),
        "events carry only the non-secret peer label"
    );
}
