//! ST-M007-006 — a **malicious** relay gains neither control nor plaintext.
//!
//! ST-M007-002/003 proved the *honest* relay carries the unmodified session
//! transparently. This suite drops that assumption: the relay here is the
//! adversary the internet path must survive (ADR-0016). It runs the **real**
//! Noise `XX` pairing and secure channel (ADR-0003) between a fresh agent and
//! console, but the byte pipe between them is a frame-aware attacker that can
//! observe, delay, drop, replay, reorder, or modify whole frames at will.
//!
//! The relay's only structural handle on the stream is the wire framing
//! (ADR-0010): a 4-byte little-endian length prefix and that many opaque body
//! bytes, identical for the handshake and the transport phase. It never holds
//! the session key, so every attack operates on ciphertext it cannot read.
//!
//! Each test lets the handshake and commit-reveal complete untouched — the
//! endpoints derive the identical Session Approval Code — and *then* arms the
//! attack on the first application frame, so the corruption is deterministic and
//! unambiguous. The guarantees asserted:
//!
//! - **Observe** — the relay forwards ciphertext only; a plaintext marker inside
//!   an encrypted message never appears in the forwarded bytes (REQ-S-005). The
//!   residual metadata it *does* learn — per-direction frame counts and byte
//!   totals — is measured and recorded, not hidden (REQ-S-021).
//! - **Delay** — arbitrary attacker-controlled latency changes nothing but
//!   timing: the SAC still matches and the marker still round-trips, and the
//!   relay still sees only ciphertext. Timing control yields no plaintext and no
//!   authority.
//! - **Drop** — a silently withheld frame delivers no message: the receiver
//!   fabricates nothing (REQ-N-008), so the relay cannot forge an outcome by
//!   suppression.
//! - **Replay / reorder / modify** — a duplicated, out-of-order, or bit-flipped
//!   transport frame fails the Noise AEAD check and permanently poisons the
//!   channel (REQ-S-008, REQ-S-009, REQ-N-008). The relay cannot inject,
//!   replay, or alter an authenticated message; the endpoint denies control and
//!   never yields the plaintext.
//!
//! Together these show relay compromise is fail-closed: it can degrade
//! availability (drop/delay) but can neither read session content nor gain
//! command authority (REQ-F-022, REQ-S-005 through REQ-S-010).
//!

// This test binary follows the crate's e2e style: `unwrap`/`expect`/`panic` are
// the assertion vocabulary.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cretlink_protocol::{Message, PROTOCOL_V1, Role as WireRole};
use cretlink_transport::{Pairing, Role, SecureChannel, SecureChannelError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, duplex};
use tokio::time::timeout;

/// The wire framing shared by the handshake and every transport record
/// (ADR-0010): a 4-byte little-endian length prefix, then that many opaque
/// body bytes. The malicious relay parses on this boundary — the only structure
/// it can see — and operates on whole ciphertext frames.
const LEN_PREFIX: usize = 4;

/// Generous per-endpoint duplex buffer so a full session's frames never block on
/// backpressure inside the in-memory pipe.
const PIPE_BYTES: usize = 32 * 1024;

/// The shared capability set both `Pairing::perform` calls hash into the Noise
/// prologue; a mismatch fails the handshake by design.
fn caps() -> Vec<String> {
    vec!["exec".to_string()]
}

/// A plaintext marker no honest relay path should ever reveal, wrapped in a real
/// application `Message` so it crosses the channel as one encrypted transport
/// frame.
const MARKER: &[u8] = b"CRETLINK-MALICIOUS-RELAY-MARKER-6d2e91";

fn marker_message() -> Message {
    Message::Hello {
        protocol: PROTOCOL_V1,
        role: WireRole::Agent,
        capabilities: vec![String::from_utf8(MARKER.to_vec()).expect("ascii marker")],
    }
}

/// A second, distinct application `Message`, used where an attack needs two
/// transport frames to act on (reorder).
fn second_message() -> Message {
    Message::Hello {
        protocol: PROTOCOL_V1,
        role: WireRole::Agent,
        capabilities: vec!["second-frame".to_string()],
    }
}

/// What the relay does with one frame it has read.
enum Action {
    /// Write these frames, in order, then keep relaying this direction.
    Forward(Vec<Vec<u8>>),
}

/// Reads one whole length-delimited frame (prefix + body) from `from`, returning
/// the full on-wire bytes. `None` at a clean or truncated end of stream, so the
/// relay winds the direction down instead of erroring.
async fn read_frame<R: AsyncRead + Unpin>(from: &mut R) -> Option<Vec<u8>> {
    let mut prefix = [0u8; LEN_PREFIX];
    if from.read_exact(&mut prefix).await.is_err() {
        return None;
    }
    let len = u32::from_le_bytes(prefix) as usize;
    let mut frame = vec![0u8; LEN_PREFIX + len];
    frame[..LEN_PREFIX].copy_from_slice(&prefix);
    if from.read_exact(&mut frame[LEN_PREFIX..]).await.is_err() {
        return None;
    }
    Some(frame)
}

/// Forwards one already-framed blob (prefix + body) verbatim.
async fn write_frame<W: AsyncWrite + Unpin>(to: &mut W, frame: &[u8]) -> std::io::Result<()> {
    to.write_all(frame).await?;
    to.flush().await
}

/// Pumps whole frames from `from` to `to`, applying `on_frame` to each and, when
/// `delay` is set, sleeping that long before forwarding — the attacker's grip on
/// timing. Ends when the source closes.
async fn pump<R, W, F>(mut from: R, mut to: W, delay: Option<Duration>, mut on_frame: F)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnMut(Vec<u8>) -> Action,
{
    while let Some(frame) = read_frame(&mut from).await {
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        let Action::Forward(frames) = on_frame(frame);
        for frame in frames {
            if write_frame(&mut to, &frame).await.is_err() {
                return;
            }
        }
    }
    let _ = to.shutdown().await;
}

/// Stands up a frame-aware malicious relay between a fresh agent/console pair.
/// The console→agent direction always passes frames through untouched; the
/// agent→console direction runs `attack`. Returns the two endpoint streams the
/// real pairing runs over. One corrupted direction is enough to prove a
/// bidirectional channel fails closed.
fn malicious_relay<F>(attack: F) -> (DuplexStream, DuplexStream)
where
    F: FnMut(Vec<u8>) -> Action + Send + 'static,
{
    let (agent_end, relay_a) = duplex(PIPE_BYTES);
    let (relay_b, console_end) = duplex(PIPE_BYTES);
    let (a_rd, a_wr) = tokio::io::split(relay_a);
    let (b_rd, b_wr) = tokio::io::split(relay_b);
    // agent → console: under attacker control.
    tokio::spawn(pump(a_rd, b_wr, None, attack));
    // console → agent: honest pass-through.
    tokio::spawn(pump(b_rd, a_wr, None, |f| Action::Forward(vec![f])));
    (agent_end, console_end)
}

/// Pairs the agent and console over `agent_end`/`console_end`, asserting the
/// untouched handshake yields a matching Session Approval Code. Returns both
/// live paired sessions for the transport-phase attack.
async fn pair_cleanly(
    agent_end: DuplexStream,
    console_end: DuplexStream,
) -> (
    cretlink_transport::Paired<DuplexStream>,
    cretlink_transport::Paired<DuplexStream>,
) {
    let shared = caps();
    let (agent, console) = tokio::join!(
        Pairing::perform(agent_end, Role::Initiator, PROTOCOL_V1, &shared),
        Pairing::perform(console_end, Role::Responder, PROTOCOL_V1, &shared),
    );
    let agent = agent.expect("agent pairs — the handshake is untouched");
    let console = console.expect("console pairs — the handshake is untouched");
    assert_eq!(
        agent.approval_code, console.approval_code,
        "the SAC matches before any transport-phase attack (REQ-S-006)"
    );
    (agent, console)
}

/// A channel that decrypted a corrupt frame is permanently poisoned: every
/// later call fails closed, so no recovery restores control (REQ-S-009).
async fn assert_poisoned(channel: &mut SecureChannel<DuplexStream>) {
    let again = channel.recv().await;
    assert!(
        matches!(again, Err(SecureChannelError::Poisoned)),
        "a channel that saw a corrupt frame stays poisoned, got {again:?}"
    );
}

/// True if `needle` occurs as a contiguous subsequence of `haystack`.
fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// **Observe.** The relay forwards ciphertext only. A plaintext marker inside an
/// encrypted application message never appears in the bytes it relays
/// (REQ-S-005), while the residual metadata it *does* learn — per-direction
/// frame counts and byte totals — is measured and recorded (REQ-S-021).
#[tokio::test]
async fn an_observing_relay_sees_only_ciphertext_and_bounded_metadata() {
    let forwarded = Arc::new(Mutex::new(Vec::<u8>::new()));
    // Residual metadata exposure: [a→c frames, a→c bytes, c→a frames, c→a bytes].
    let meta = Arc::new(Mutex::new([0u64; 4]));

    let (agent_end, relay_a) = duplex(PIPE_BYTES);
    let (relay_b, console_end) = duplex(PIPE_BYTES);
    let (a_rd, a_wr) = tokio::io::split(relay_a);
    let (b_rd, b_wr) = tokio::io::split(relay_b);

    let log_ac = Arc::clone(&forwarded);
    let meta_ac = Arc::clone(&meta);
    tokio::spawn(pump(a_rd, b_wr, None, move |frame| {
        {
            let mut m = meta_ac.lock().unwrap();
            m[0] += 1;
            m[1] += frame.len() as u64;
        }
        log_ac.lock().unwrap().extend_from_slice(&frame);
        Action::Forward(vec![frame])
    }));
    let log_ca = Arc::clone(&forwarded);
    let meta_ca = Arc::clone(&meta);
    tokio::spawn(pump(b_rd, a_wr, None, move |frame| {
        {
            let mut m = meta_ca.lock().unwrap();
            m[2] += 1;
            m[3] += frame.len() as u64;
        }
        log_ca.lock().unwrap().extend_from_slice(&frame);
        Action::Forward(vec![frame])
    }));

    let (mut agent, mut console) = pair_cleanly(agent_end, console_end).await;

    // The marker crosses end to end in cleartext at the endpoints.
    agent
        .channel
        .send(&marker_message())
        .await
        .expect("agent sends the marker");
    let received = console
        .channel
        .recv()
        .await
        .expect("console reads the marker");
    assert!(
        matches!(received, Message::Hello { capabilities, .. }
            if capabilities == vec![String::from_utf8(MARKER.to_vec()).unwrap()]),
        "the marker round-trips intact end to end"
    );

    // Close both legs so the relay drains and its record is complete.
    drop(agent);
    drop(console);
    tokio::task::yield_now().await;

    let seen = forwarded.lock().unwrap().clone();
    assert!(!seen.is_empty(), "the relay did forward bytes");
    assert!(
        !contains_subsequence(&seen, MARKER),
        "the marker crossed the relay only as ciphertext (REQ-S-005)"
    );

    // Residual metadata the relay legitimately learns — and the ceiling on it.
    let [ac_frames, ac_bytes, ca_frames, ca_bytes] = *meta.lock().unwrap();
    assert!(
        ac_frames > 0 && ca_frames > 0,
        "the relay observes that framed traffic flowed in both directions (residual metadata, REQ-S-021)"
    );
    assert!(
        ac_bytes > 0 && ca_bytes > 0,
        "the relay observes ciphertext byte volume in both directions (residual metadata, REQ-S-021)"
    );
}

/// **Delay.** Arbitrary attacker-controlled latency on both directions changes
/// nothing but timing: the SAC still matches, the marker still round-trips, and
/// the relay still sees only ciphertext. Timing control yields no plaintext and
/// no authority.
#[tokio::test]
async fn a_delaying_relay_gains_nothing_but_latency() {
    let forwarded = Arc::new(Mutex::new(Vec::<u8>::new()));

    let (agent_end, relay_a) = duplex(PIPE_BYTES);
    let (relay_b, console_end) = duplex(PIPE_BYTES);
    let (a_rd, a_wr) = tokio::io::split(relay_a);
    let (b_rd, b_wr) = tokio::io::split(relay_b);

    // A per-frame delay in *both* directions — the attacker controls all timing.
    let delay = Some(Duration::from_millis(3));
    let log_ac = Arc::clone(&forwarded);
    tokio::spawn(pump(a_rd, b_wr, delay, move |frame| {
        log_ac.lock().unwrap().extend_from_slice(&frame);
        Action::Forward(vec![frame])
    }));
    let log_ca = Arc::clone(&forwarded);
    tokio::spawn(pump(b_rd, a_wr, delay, move |frame| {
        log_ca.lock().unwrap().extend_from_slice(&frame);
        Action::Forward(vec![frame])
    }));

    // Pairing still completes across the injected latency, with a matching SAC.
    let (mut agent, mut console) = pair_cleanly(agent_end, console_end).await;

    agent
        .channel
        .send(&marker_message())
        .await
        .expect("agent sends the marker despite delay");
    let received = console
        .channel
        .recv()
        .await
        .expect("console reads the marker");
    assert!(
        matches!(received, Message::Hello { capabilities, .. }
            if capabilities == vec![String::from_utf8(MARKER.to_vec()).unwrap()]),
        "delay changes only timing — the marker still round-trips"
    );

    drop(agent);
    drop(console);
    tokio::task::yield_now().await;
    let seen = forwarded.lock().unwrap().clone();
    assert!(
        !contains_subsequence(&seen, MARKER),
        "a delaying relay still sees only ciphertext (REQ-S-005)"
    );
}

/// **Drop.** A silently withheld application frame delivers no message: the
/// receiver waits and fabricates nothing (REQ-N-008). The relay cannot forge an
/// outcome by suppression, and it never learns the plaintext.
#[tokio::test]
async fn a_dropping_relay_delivers_no_fabricated_message() {
    let armed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&armed);
    let (agent_end, console_end) = malicious_relay(move |frame| {
        if flag.swap(false, Ordering::SeqCst) {
            // Swallow the armed application frame; forward nothing.
            Action::Forward(vec![])
        } else {
            Action::Forward(vec![frame])
        }
    });

    let (mut agent, mut console) = pair_cleanly(agent_end, console_end).await;

    // Arm the drop, then send exactly one application frame: the relay eats it.
    armed.store(true, Ordering::SeqCst);
    agent
        .channel
        .send(&marker_message())
        .await
        .expect("agent's send completes into the pipe");

    // The console must not receive anything — a dropped frame is not a delivered
    // one, and nothing is fabricated to stand in for it.
    let outcome = timeout(Duration::from_millis(300), console.channel.recv()).await;
    assert!(
        outcome.is_err(),
        "a silently dropped frame delivers no message (no plaintext, no forgery), got {outcome:?}"
    );
}

/// **Modify.** A single bit-flipped ciphertext byte in a transport frame fails
/// the Noise AEAD check: the receiver's `recv` fails closed and the channel is
/// poisoned (REQ-S-008, REQ-S-009). The relay cannot alter an authenticated
/// message, and the flip never yields the plaintext.
#[tokio::test]
async fn a_modified_transport_frame_fails_closed() {
    let armed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&armed);
    let (agent_end, console_end) = malicious_relay(move |mut frame| {
        if flag.swap(false, Ordering::SeqCst) {
            // Flip the first ciphertext byte (just past the length prefix); the
            // AEAD tag no longer verifies.
            frame[LEN_PREFIX] ^= 0xff;
        }
        Action::Forward(vec![frame])
    });

    let (mut agent, mut console) = pair_cleanly(agent_end, console_end).await;

    armed.store(true, Ordering::SeqCst);
    agent
        .channel
        .send(&marker_message())
        .await
        .expect("agent sends one app frame");

    let outcome = console.channel.recv().await;
    assert!(
        matches!(outcome, Err(SecureChannelError::Noise(_))),
        "a modified ciphertext frame fails closed on the AEAD tag, got {outcome:?}"
    );
    assert_poisoned(&mut console.channel).await;
}

/// **Replay.** A transport frame delivered twice is accepted at most once: the
/// duplicate decrypts under the wrong receive nonce, fails the AEAD check, and
/// poisons the channel (REQ-S-008). The relay cannot replay an authenticated
/// message to repeat its effect.
#[tokio::test]
async fn a_replayed_transport_frame_fails_closed() {
    let armed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&armed);
    let (agent_end, console_end) = malicious_relay(move |frame| {
        if flag.swap(false, Ordering::SeqCst) {
            // Deliver the armed frame, then deliver it a second time.
            Action::Forward(vec![frame.clone(), frame])
        } else {
            Action::Forward(vec![frame])
        }
    });

    let (mut agent, mut console) = pair_cleanly(agent_end, console_end).await;

    armed.store(true, Ordering::SeqCst);
    agent
        .channel
        .send(&marker_message())
        .await
        .expect("agent sends one app frame");

    // The first copy decrypts once, in order.
    let first = console.channel.recv().await;
    assert!(
        matches!(first, Ok(Message::Hello { ref capabilities, .. })
            if *capabilities == vec![String::from_utf8(MARKER.to_vec()).unwrap()]),
        "the genuine frame is accepted exactly once, got {first:?}"
    );
    // The replayed copy carries a stale nonce and fails closed.
    let replayed = console.channel.recv().await;
    assert!(
        matches!(replayed, Err(SecureChannelError::Noise(_))),
        "a replayed frame fails the AEAD check under the advanced nonce, got {replayed:?}"
    );
    assert_poisoned(&mut console.channel).await;
}

/// **Reorder.** Two transport frames delivered out of order fail closed: the
/// later frame decrypts under the earlier receive nonce, fails the AEAD check,
/// and poisons the channel (REQ-S-008, REQ-S-009). The relay cannot reshuffle
/// the authenticated stream.
#[tokio::test]
async fn a_reordered_transport_frame_fails_closed() {
    let armed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&armed);
    let mut held: Option<Vec<u8>> = None;
    let (agent_end, console_end) = malicious_relay(move |frame| {
        if flag.load(Ordering::SeqCst) {
            match held.take() {
                // Hold the first armed frame back, forwarding nothing yet.
                None => {
                    held = Some(frame);
                    Action::Forward(vec![])
                }
                // On the second, deliver it first and the held one after —
                // reversing wire order.
                Some(first) => {
                    flag.store(false, Ordering::SeqCst);
                    Action::Forward(vec![frame, first])
                }
            }
        } else {
            Action::Forward(vec![frame])
        }
    });

    let (mut agent, mut console) = pair_cleanly(agent_end, console_end).await;

    armed.store(true, Ordering::SeqCst);
    agent
        .channel
        .send(&marker_message())
        .await
        .expect("agent sends frame A");
    agent
        .channel
        .send(&second_message())
        .await
        .expect("agent sends frame B");

    // The console receives B before A; B's nonce does not match the receiver's
    // in-order counter, so the very first decrypt fails closed.
    let outcome = console.channel.recv().await;
    assert!(
        matches!(outcome, Err(SecureChannelError::Noise(_))),
        "an out-of-order frame fails the AEAD check under the in-order nonce, got {outcome:?}"
    );
    assert_poisoned(&mut console.channel).await;
}
