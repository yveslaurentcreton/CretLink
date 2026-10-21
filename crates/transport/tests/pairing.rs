//! Adversarial integration tests for the Noise `XX` pairing and the encrypted
//! transport (ST-M002-004, ADR-0003, REQ-S-005 through REQ-S-010).
//!
//! These drive the public [`Pairing`]/[`SecureChannel`] API over in-memory
//! `tokio::io::duplex` streams, including a frame-level relay that lets a test
//! tamper with, replay, or oversize the ciphertext after pairing completes.

// This is a test binary: `unwrap`/`expect` are the standard way to assert.
// `allow-unwrap-in-tests` only reaches `#[test]` bodies, not the shared relay
// and framing helpers below, so allow them for the whole test crate.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use cretlink_protocol::{
    Message, Operation, PROTOCOL_V1, ProtocolVersion, RequestId, SessionId, Shell, limits,
};
use cretlink_transport::{
    MAX_CIPHERTEXT_LEN, Paired, Pairing, PairingError, Role, SecureChannelError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex, split};
use tokio::sync::Mutex;

fn caps(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn request(id: u64, op: Operation) -> Message {
    Message::Request {
        id: RequestId(id),
        session: SessionId::new(),
        origin: cretlink_protocol::Origin::Console,
        op,
    }
}

/// Runs a full, honest pairing over a direct duplex and returns both sides.
async fn pair_directly(
    capabilities: &[String],
) -> (
    Result<Paired<DuplexStream>, PairingError>,
    Result<Paired<DuplexStream>, PairingError>,
) {
    let (a, b) = duplex(1 << 16);
    tokio::join!(
        Pairing::perform(a, Role::Initiator, PROTOCOL_V1, capabilities),
        Pairing::perform(b, Role::Responder, PROTOCOL_V1, capabilities),
    )
}

// --- happy path -----------------------------------------------------------

#[tokio::test]
async fn happy_path_same_code_and_bidirectional_messages() {
    let (initiator, responder) = pair_directly(&caps(&["exec", "jobs"])).await;
    let mut initiator = initiator.expect("initiator pairs");
    let mut responder = responder.expect("responder pairs");

    // Both endpoints derive the identical six-digit code (REQ-S-006).
    assert_eq!(initiator.approval_code, responder.approval_code);
    assert_eq!(initiator.approval_code.to_string().len(), 6);

    // Each side learned the other's fresh static key, and the two differ.
    assert_ne!(initiator.peer_static, responder.peer_static);
    assert_ne!(initiator.peer_static, [0u8; 32]);

    // Both endpoints hold the identical, non-zero Noise transcript hash `h`,
    // the same root the approval code binds to. A consented session derives the
    // reboot-resume continuity key from it (ADR-0014, ST-M006-001).
    assert_eq!(*initiator.handshake_hash, *responder.handshake_hash);
    assert_ne!(*initiator.handshake_hash, [0u8; 32]);

    // Confidential, authenticated traffic flows both ways (REQ-S-005).
    let up = request(1, Operation::ListJobs {});
    initiator.channel.send(&up).await.expect("send up");
    assert_eq!(responder.channel.recv().await.expect("recv up"), up);

    let down = request(2, Operation::SystemInfo {});
    responder.channel.send(&down).await.expect("send down");
    assert_eq!(initiator.channel.recv().await.expect("recv down"), down);
}

// --- MITM: the core REQ-S-006 test ---------------------------------------

#[tokio::test]
async fn mitm_produces_different_approval_codes() {
    // A man-in-the-middle terminates the victim initiator's channel (acting as
    // responder with its own keys) and separately dials the victim responder
    // (acting as initiator with its own keys). Both handshakes succeed, but the
    // two victims bind to different transcripts.
    let c = caps(&["exec"]);
    let (vi_io, mitm_to_vi) = duplex(1 << 16);
    let (mitm_to_vr, vr_io) = duplex(1 << 16);

    let (vi, mitm_r, mitm_i, vr) = tokio::join!(
        Pairing::perform(vi_io, Role::Initiator, PROTOCOL_V1, &c),
        Pairing::perform(mitm_to_vi, Role::Responder, PROTOCOL_V1, &c),
        Pairing::perform(mitm_to_vr, Role::Initiator, PROTOCOL_V1, &c),
        Pairing::perform(vr_io, Role::Responder, PROTOCOL_V1, &c),
    );
    let vi = vi.expect("victim initiator pairs with MITM");
    let mitm_r = mitm_r.expect("MITM pairs with victim initiator");
    let mitm_i = mitm_i.expect("MITM pairs with victim responder");
    let vr = vr.expect("victim responder pairs with MITM");

    // Each real leg agrees internally...
    assert_eq!(vi.approval_code, mitm_r.approval_code);
    assert_eq!(mitm_i.approval_code, vr.approval_code);

    // ...but the two humans see DIFFERENT codes, so the on-screen comparison
    // catches the MITM (collision probability is 1e-6). This is the human
    // trust root the whole design rests on (REQ-S-006).
    assert_ne!(vi.approval_code, vr.approval_code);

    // The transcript hashes diverge for the same reason, so a reboot-resume key
    // derived from `h` on one leg can never match the other (ADR-0014).
    assert_ne!(*vi.handshake_hash, *vr.handshake_hash);
}

// --- downgrade / incompatible parameters fail closed (REQ-S-008) ----------

#[tokio::test]
async fn trimmed_capability_set_fails_closed() {
    // Responder advertises fewer capabilities than the initiator: the prologues
    // differ, so the authenticated handshake fails and yields no channel.
    let (a, b) = duplex(1 << 16);
    let initiator_caps = caps(&["exec", "jobs"]);
    let responder_caps = caps(&["exec"]);
    let (initiator, responder) = tokio::join!(
        Pairing::perform(a, Role::Initiator, PROTOCOL_V1, &initiator_caps),
        Pairing::perform(b, Role::Responder, PROTOCOL_V1, &responder_caps),
    );
    assert!(matches!(initiator, Err(PairingError::HandshakeFailed(_))));
    assert!(responder.is_err());
}

#[tokio::test]
async fn incompatible_version_fails_closed() {
    let (a, b) = duplex(1 << 16);
    let c = caps(&["exec"]);
    let (initiator, responder) = tokio::join!(
        Pairing::perform(a, Role::Initiator, ProtocolVersion::new(1, 0), &c),
        Pairing::perform(b, Role::Responder, ProtocolVersion::new(2, 0), &c),
    );
    assert!(matches!(initiator, Err(PairingError::HandshakeFailed(_))));
    assert!(responder.is_err());
}

#[tokio::test]
async fn locally_oversized_capability_set_is_refused_before_io() {
    // More than MAX_CAPABILITIES entries would be silently clamped by the
    // prologue builder; refuse up front so we never advertise a set we did not
    // mean (a self-inflicted downgrade). No handshake byte is sent.
    let (a, _b) = duplex(64);
    let many: Vec<String> = (0..=limits::MAX_CAPABILITIES)
        .map(|i| format!("cap{i:03}"))
        .collect();
    let result = Pairing::perform(a, Role::Initiator, PROTOCOL_V1, &many).await;
    assert!(matches!(
        result,
        Err(PairingError::VersionCapabilityMismatch)
    ));
}

// --- ephemeral keys: fresh static per session (REQ-S-007) -----------------

#[tokio::test]
async fn each_session_uses_a_fresh_static_key() {
    let c = caps(&["exec"]);
    let (i1, r1) = pair_directly(&c).await;
    let (i2, r2) = pair_directly(&c).await;
    let (i1, r1) = (i1.expect("pair 1 initiator"), r1.expect("pair 1 responder"));
    let (i2, r2) = (i2.expect("pair 2 initiator"), r2.expect("pair 2 responder"));

    // i.peer_static is the responder's static; r.peer_static is the
    // initiator's. Both must be freshly generated every session.
    assert_ne!(
        i1.peer_static, i2.peer_static,
        "responder reused a static key"
    );
    assert_ne!(
        r1.peer_static, r2.peer_static,
        "initiator reused a static key"
    );
    // The codes are independent per session too.
    assert_eq!(i1.approval_code, r1.approval_code);
    assert_eq!(i2.approval_code, r2.approval_code);
}

// --- frame-level relay for tamper / replay / oversized tests --------------

/// What the A->B relay direction should do with the next frame it forwards.
/// Consumed once and reset to [`RelayAction::Pass`] after acting.
#[derive(Debug, Clone, Copy)]
enum RelayAction {
    Pass,
    TamperNext,
    DuplicateNext,
    InjectOversized,
    /// Record the next forwarded frame (then pass it through unchanged) so a
    /// test can inspect the on-wire ciphertext for plaintext leakage.
    SnoopNext,
}

async fn read_frame<R: AsyncReadExt + Unpin>(io: &mut R) -> Option<Vec<u8>> {
    let mut prefix = [0u8; 4];
    io.read_exact(&mut prefix).await.ok()?;
    let len = u32::from_le_bytes(prefix) as usize;
    let mut buf = vec![0u8; len];
    io.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

async fn write_frame<W: AsyncWriteExt + Unpin>(io: &mut W, payload: &[u8]) {
    io.write_all(&u32::try_from(payload.len()).unwrap().to_le_bytes())
        .await
        .unwrap();
    io.write_all(payload).await.unwrap();
    io.flush().await.unwrap();
}

/// A paired session whose two channels are joined by a controllable frame
/// relay rather than a direct duplex, so ciphertext can be manipulated in
/// transit after pairing.
struct Relayed {
    initiator: Paired<DuplexStream>,
    responder: Paired<DuplexStream>,
    action: Arc<Mutex<RelayAction>>,
    /// Frames recorded by [`RelayAction::SnoopNext`], in forwarding order.
    captured: Arc<Mutex<Vec<Vec<u8>>>>,
}

async fn pair_over_relay() -> Relayed {
    let (a_io, a_wire) = duplex(1 << 16);
    let (b_wire, b_io) = duplex(1 << 16);
    let (mut a_rd, mut a_wr) = split(a_wire);
    let (mut b_rd, mut b_wr) = split(b_wire);

    let action = Arc::new(Mutex::new(RelayAction::Pass));
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

    // A -> B, with the tamper/replay/oversize/snoop hook.
    let action_ab = Arc::clone(&action);
    let captured_ab = Arc::clone(&captured);
    tokio::spawn(async move {
        while let Some(frame) = read_frame(&mut a_rd).await {
            let act = {
                let mut guard = action_ab.lock().await;
                std::mem::replace(&mut *guard, RelayAction::Pass)
            };
            match act {
                RelayAction::Pass => write_frame(&mut b_wr, &frame).await,
                RelayAction::TamperNext => {
                    let mut tampered = frame.clone();
                    let last = tampered.len() - 1;
                    tampered[last] ^= 0xff;
                    write_frame(&mut b_wr, &tampered).await;
                }
                RelayAction::DuplicateNext => {
                    write_frame(&mut b_wr, &frame).await;
                    write_frame(&mut b_wr, &frame).await;
                }
                RelayAction::InjectOversized => {
                    // Discard the real frame; send only an over-large length
                    // prefix so the receiver rejects it before allocating.
                    let declared = u32::try_from(MAX_CIPHERTEXT_LEN + 1).unwrap();
                    b_wr.write_all(&declared.to_le_bytes()).await.unwrap();
                    b_wr.flush().await.unwrap();
                }
                RelayAction::SnoopNext => {
                    // Record the ciphertext, then forward it untouched.
                    captured_ab.lock().await.push(frame.clone());
                    write_frame(&mut b_wr, &frame).await;
                }
            }
        }
    });

    // B -> A, always transparent.
    tokio::spawn(async move {
        while let Some(frame) = read_frame(&mut b_rd).await {
            write_frame(&mut a_wr, &frame).await;
        }
    });

    let c = caps(&["exec"]);
    let (initiator, responder) = tokio::join!(
        Pairing::perform(a_io, Role::Initiator, PROTOCOL_V1, &c),
        Pairing::perform(b_io, Role::Responder, PROTOCOL_V1, &c),
    );
    let initiator = initiator.expect("initiator pairs over relay");
    let responder = responder.expect("responder pairs over relay");
    assert_eq!(initiator.approval_code, responder.approval_code);

    Relayed {
        initiator,
        responder,
        action,
        captured,
    }
}

#[tokio::test]
async fn tampered_ciphertext_poisons_the_channel() {
    let mut session = pair_over_relay().await;
    *session.action.lock().await = RelayAction::TamperNext;

    session
        .initiator
        .channel
        .send(&request(1, Operation::ListJobs {}))
        .await
        .expect("initiator send succeeds; tamper happens in transit");

    // Decryption of the tampered frame fails and the channel is poisoned.
    let err = session.responder.channel.recv().await.unwrap_err();
    assert!(matches!(err, SecureChannelError::Noise(_)));
    assert!(session.responder.channel.is_poisoned());

    // Every subsequent operation fails closed (REQ-S-009).
    assert!(matches!(
        session.responder.channel.recv().await.unwrap_err(),
        SecureChannelError::Poisoned
    ));
    assert!(matches!(
        session
            .responder
            .channel
            .send(&request(2, Operation::ListJobs {}))
            .await
            .unwrap_err(),
        SecureChannelError::Poisoned
    ));
}

#[tokio::test]
async fn replayed_ciphertext_is_rejected_and_poisons_the_channel() {
    let mut session = pair_over_relay().await;
    *session.action.lock().await = RelayAction::DuplicateNext;

    let msg = request(1, Operation::ListJobs {});
    session
        .initiator
        .channel
        .send(&msg)
        .await
        .expect("initiator send");

    // First copy decrypts under nonce 0.
    assert_eq!(
        session.responder.channel.recv().await.expect("first copy"),
        msg
    );
    // The replayed copy is rejected: snow's receive nonce has advanced, so the
    // authentication tag no longer verifies (REQ-S-008).
    let err = session.responder.channel.recv().await.unwrap_err();
    assert!(matches!(err, SecureChannelError::Noise(_)));
    assert!(session.responder.channel.is_poisoned());
}

#[tokio::test]
async fn oversized_transport_frame_is_rejected_before_allocation() {
    let mut session = pair_over_relay().await;
    *session.action.lock().await = RelayAction::InjectOversized;

    // Trigger the relay with any frame; it replaces it with an over-large
    // length prefix aimed at the responder.
    session
        .initiator
        .channel
        .send(&request(1, Operation::ListJobs {}))
        .await
        .expect("initiator send");

    let err = session.responder.channel.recv().await.unwrap_err();
    assert!(matches!(
        err,
        SecureChannelError::Oversized { len, max }
            if len == MAX_CIPHERTEXT_LEN + 1 && max == MAX_CIPHERTEXT_LEN
    ));
    assert!(session.responder.channel.is_poisoned());
}

// --- confidentiality by packet inspection (REQ-S-005, ST-M002-004 AC3) -----

/// True if `needle` occurs as a contiguous subslice of `haystack`.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[tokio::test]
async fn paired_ciphertext_frame_does_not_leak_plaintext() {
    // The story's verification calls for packet inspection: after pairing, the
    // on-wire frames must be opaque. Send a message carrying a distinctive
    // marker, capture the forwarded ciphertext frame, and assert that neither
    // the marker nor the full encoded plaintext appears in it. Integrity is
    // covered by the tamper/replay tests above; this is the confidentiality
    // half of AC3 (REQ-S-005).
    let mut session = pair_over_relay().await;
    *session.action.lock().await = RelayAction::SnoopNext;

    const MARKER: &str = "cretlink-confidential-marker-4f2a";
    let msg = request(
        1,
        Operation::Exec {
            shell: Shell::PowerShell,
            command: MARKER.to_string(),
            working_dir: None,
            env: Default::default(),
            timeout_ms: None,
        },
    );
    let plaintext = msg.encode().expect("encode message");
    // The marker really is in the plaintext, so its absence from the ciphertext
    // is a meaningful assertion rather than a vacuous one.
    assert!(contains_subslice(&plaintext, MARKER.as_bytes()));

    session
        .initiator
        .channel
        .send(&msg)
        .await
        .expect("initiator send");
    assert_eq!(session.responder.channel.recv().await.expect("recv"), msg);

    let captured = session.captured.lock().await;
    let frame = captured
        .first()
        .expect("relay captured a forwarded ciphertext frame");
    assert!(
        !contains_subslice(frame, &plaintext),
        "encoded plaintext leaked into the on-wire frame"
    );
    assert!(
        !contains_subslice(frame, MARKER.as_bytes()),
        "plaintext marker leaked into the on-wire frame"
    );
    // Sanity: the frame is a real ChaCha20-Poly1305 record (payload + 16-byte
    // tag), not an accidental passthrough of the plaintext.
    assert!(frame.len() > plaintext.len());
}
