//! End-to-end proof that the relay is an opaque byte pipe (ST-M007-002).
//!
//! These tests drive the *real* Noise `XX` pairing and secure channel
//! (ADR-0003) across the relay [`splice`], establishing that the relay:
//!
//! 1. lets two endpoints complete the identical attended session — same Session
//!    Approval Code — while it only ever moves ciphertext;
//! 2. never sees session plaintext (a marker embedded in an encrypted
//!    application message never appears in the bytes the relay forwards); and
//! 3. cannot synthesize or alter an authenticated endpoint — a single flipped
//!    handshake byte makes the pairing fail closed, never silently succeed.
//!
//! A fourth test composes the [`Rendezvous`] matcher with the splice, proving
//! the two arrivals meet on an addressing-only ticket and then pair E2E.
//!

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use cretlink_protocol::{Message, PROTOCOL_V1, Role as WireRole};
use cretlink_relay::{Matched, Rendezvous, RendezvousTicket, SpliceLimits, StopSignal, splice};
use cretlink_transport::{Pairing, Role};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf, duplex};

/// Both `Pairing::perform` calls must hash the same capability set into the
/// prologue or the handshake fails by design; this is that shared set.
fn caps() -> Vec<String> {
    vec!["exec".to_string()]
}

/// The unmodified end-to-end Noise session pairs across the relay and both
/// endpoints derive the identical Session Approval Code — the relay adds no leg
/// of its own to the transcript.
#[tokio::test]
async fn pairing_succeeds_and_sac_matches_through_the_splice() {
    let (agent_end, relay_a) = duplex(4096);
    let (relay_b, console_end) = duplex(4096);

    let (_signal, stop) = StopSignal::new();
    let relay = tokio::spawn(splice(relay_a, relay_b, SpliceLimits::default(), stop));

    let shared = caps();
    let (agent, console) = tokio::join!(
        Pairing::perform(agent_end, Role::Initiator, PROTOCOL_V1, &shared),
        Pairing::perform(console_end, Role::Responder, PROTOCOL_V1, &shared),
    );
    let agent = agent.expect("agent pairs through the relay");
    let console = console.expect("console pairs through the relay");

    assert_eq!(
        agent.approval_code, console.approval_code,
        "the SAC is identical on both untampered endpoints across the relay"
    );
    assert_eq!(
        *agent.handshake_hash, *console.handshake_hash,
        "the transcript hash matches, so no relay leg entered the handshake"
    );

    // Dropping the paired endpoints closes both relay legs; the splice ends.
    drop(agent);
    drop(console);
    let report = relay.await.expect("relay task joins");
    assert!(report.a_to_b_frames > 0 && report.b_to_a_frames > 0);
}

/// A marker carried inside an *encrypted* application message never appears in
/// the bytes the relay forwards: the relay sees ciphertext only (REQ-S-005).
#[tokio::test]
async fn relay_forwards_only_ciphertext() {
    const MARKER: &[u8] = b"CRETLINK-PLAINTEXT-MARKER-a17f3c";

    let (agent_end, relay_a) = duplex(8192);
    let (relay_b, console_end) = duplex(8192);

    // Record every byte the relay reads on each leg — i.e. everything it
    // forwards, in both directions.
    let log = Arc::new(Mutex::new(Vec::new()));
    let relay_a = Recorder::new(relay_a, Arc::clone(&log));
    let relay_b = Recorder::new(relay_b, Arc::clone(&log));

    let (_signal, stop) = StopSignal::new();
    let relay = tokio::spawn(splice(relay_a, relay_b, SpliceLimits::default(), stop));

    let shared = caps();
    let (agent, console) = tokio::join!(
        Pairing::perform(agent_end, Role::Initiator, PROTOCOL_V1, &shared),
        Pairing::perform(console_end, Role::Responder, PROTOCOL_V1, &shared),
    );
    let mut agent = agent.expect("agent pairs");
    let mut console = console.expect("console pairs");

    // Send the marker as an encrypted application message over the live channel.
    let carrier = Message::Hello {
        protocol: PROTOCOL_V1,
        role: WireRole::Agent,
        capabilities: vec![String::from_utf8(MARKER.to_vec()).expect("ascii marker")],
    };
    agent.channel.send(&carrier).await.expect("send marker");
    let received = console.channel.recv().await.expect("recv marker");
    assert!(
        matches!(received, Message::Hello { capabilities, .. } if capabilities == vec![String::from_utf8(MARKER.to_vec()).unwrap()]),
        "the endpoints exchange the marker in cleartext at their ends"
    );

    drop(agent);
    drop(console);
    let _ = relay.await;

    let forwarded = log.lock().expect("log lock").clone();
    assert!(!forwarded.is_empty(), "the relay did forward bytes");
    assert!(
        !contains_subsequence(&forwarded, MARKER),
        "the plaintext marker must never appear in what the relay forwards"
    );
}

/// One flipped byte in a forwarded handshake message makes the pairing fail
/// closed on at least one endpoint: the relay cannot alter the session and pass
/// it off as authentic (REQ-S-005, REQ-N-008).
#[tokio::test]
async fn handshake_tampering_fails_closed() {
    let (agent_end, relay_a) = duplex(4096);
    let (relay_b, console_end) = duplex(4096);

    // Flip a byte inside the agent's very first handshake message (offset 10 is
    // within the plaintext ephemeral public key that follows the 4-byte length
    // prefix), corrupting the transcript so the AEAD checks fail.
    let relay_a = Tamper::new(relay_a, 10);

    let (_signal, stop) = StopSignal::new();
    tokio::spawn(splice(relay_a, relay_b, SpliceLimits::default(), stop));

    let shared = caps();
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            Pairing::perform(agent_end, Role::Initiator, PROTOCOL_V1, &shared),
            Pairing::perform(console_end, Role::Responder, PROTOCOL_V1, &shared),
        )
    })
    .await;

    match outcome {
        Ok((agent, console)) => assert!(
            agent.is_err() || console.is_err(),
            "a tampered handshake must not yield a paired session"
        ),
        // A stall is also fail-closed: no side produced an authenticated pairing.
        Err(_elapsed) => {}
    }
}

/// The rendezvous matcher meets the two outbound arrivals on one addressing-only
/// ticket, and the resulting pair splices into a working E2E Noise session.
#[tokio::test]
async fn rendezvous_then_splice_pairs_end_to_end() {
    let rv: Arc<Rendezvous<DuplexStream>> = Arc::new(Rendezvous::new());
    let ticket = RendezvousTicket::generate().expect("ticket");

    let (agent_end, relay_a) = duplex(4096);
    let (relay_b, console_end) = duplex(4096);

    // Two independent relay ingress tasks join on the same ticket.
    let join_a = {
        let rv = Arc::clone(&rv);
        let ticket = ticket.clone();
        tokio::spawn(async move { rv.join(ticket, relay_a, Duration::from_secs(5)).await })
    };
    let join_b = {
        let rv = Arc::clone(&rv);
        let ticket = ticket.clone();
        tokio::spawn(async move { rv.join(ticket, relay_b, Duration::from_secs(5)).await })
    };

    let r_a = join_a.await.expect("join a task").expect("join a ok");
    let r_b = join_b.await.expect("join b task").expect("join b ok");

    // Exactly one arrival carries the matched pair; the other parked.
    let (leg1, leg2) = match (r_a, r_b) {
        (Matched::Pair { first, second }, Matched::Parked)
        | (Matched::Parked, Matched::Pair { first, second }) => (first, second),
        _ => panic!("exactly one side should carry the matched pair"),
    };

    let (_signal, stop) = StopSignal::new();
    tokio::spawn(splice(leg1, leg2, SpliceLimits::default(), stop));

    let shared = caps();
    let (agent, console) = tokio::join!(
        Pairing::perform(agent_end, Role::Initiator, PROTOCOL_V1, &shared),
        Pairing::perform(console_end, Role::Responder, PROTOCOL_V1, &shared),
    );
    assert_eq!(
        agent.expect("agent pairs").approval_code,
        console.expect("console pairs").approval_code,
        "matched-then-spliced endpoints reach the same attended session"
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

/// A pass-through stream that flips a single byte at a fixed read offset,
/// standing in for a relay that tries to alter the session.
struct Tamper<S> {
    inner: S,
    /// Absolute offset (in bytes read) to corrupt.
    target: u64,
    /// Bytes read so far.
    seen: u64,
    /// Whether the flip has been applied.
    done: bool,
}

impl<S> Tamper<S> {
    fn new(inner: S, target: u64) -> Self {
        Self {
            inner,
            target,
            seen: 0,
            done: false,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Tamper<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &res {
            let read_now = (buf.filled().len() - before) as u64;
            if read_now > 0 && !self.done {
                let start = self.seen;
                let end = start + read_now;
                if self.target >= start && self.target < end {
                    let idx = before + (self.target - start) as usize;
                    buf.filled_mut()[idx] ^= 0xff;
                    self.done = true;
                }
                self.seen = end;
            }
        }
        res
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tamper<S> {
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
