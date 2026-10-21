//! The opaque bidirectional frame forwarder (ST-M007-002, ADR-0016).
//!
//! [`splice`] joins two already-connected byte streams — the relay's leg to each
//! endpoint — into one session pipe. It moves the endpoints' unmodified
//! length-prefixed Noise frames (ADR-0010) in both directions **without ever
//! interpreting a body**. It never holds a session key, never decodes a
//! [`cretlink_protocol::Message`], and never fabricates a clean outcome: any
//! fault tears both directions down fail-closed and is reported honestly
//! (REQ-N-008).
//!
//! # Flow control (REQ-N-006)
//!
//! Each direction reads exactly one whole frame, then writes it in full before
//! reading the next. At most one frame — bounded by [`MAX_RELAY_FRAME_BYTES`] —
//! is buffered per direction. A slow reader backpressures its writer: the relay
//! stops pulling the next frame from the source until the destination has
//! drained the current one, so a fast sender cannot force unbounded relay
//! memory.
//!
//! # Quotas (REQ-N-006)
//!
//! [`SpliceLimits`] caps the per-direction frame size, total bytes, and total
//! frames. Hitting any cap stops forwarding fail-closed with a
//! [`SpliceOutcome::QuotaExceeded`] (or [`SpliceOutcome::FrameOversize`]).
//!
//! # Observability
//!
//! [`splice`] returns a [`SpliceReport`]: per-direction byte and frame counts
//! plus the terminal [`SpliceOutcome`]. These are metadata only — counts and an
//! outcome tag — never session content.
//!

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::cancel::StopHandle;

/// The largest single frame the relay will forward, in bytes.
///
/// It matches the transport's maximum ciphertext record: the plaintext frame
/// ceiling ([`cretlink_protocol::limits::MAX_FRAME_BYTES`]) plus the 16-byte
/// Noise AEAD tag, the same bound the endpoints enforce on their own reads. The
/// relay does not interpret the body, but it does reject an over-ceiling length
/// prefix **before** allocating the buffer, so a hostile prefix fails closed
/// without memory cost (REQ-N-012).
pub const MAX_RELAY_FRAME_BYTES: usize = cretlink_protocol::limits::MAX_FRAME_BYTES + 16;

/// The frame length prefix is a 4-byte little-endian `u32`, matching the
/// transport framing (ADR-0010). The prefix is transport framing, deliberately
/// outside the endpoints' encryption; the relay reads it only to find frame
/// boundaries and never looks inside the body it delimits.
const LENGTH_PREFIX_LEN: usize = 4;

/// Which leg of the session a forwarded frame travelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// From the first endpoint (`a`) toward the second (`b`).
    AtoB,
    /// From the second endpoint (`b`) toward the first (`a`).
    BtoA,
}

/// Per-direction forwarding bounds. Construct with [`SpliceLimits::default`] for
/// the frame-size ceiling only, then tighten the byte and frame quotas for an
/// exposed relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpliceLimits {
    /// Largest single frame accepted, in bytes. Defaults to
    /// [`MAX_RELAY_FRAME_BYTES`].
    pub max_frame_bytes: usize,
    /// Optional cap on total wire bytes forwarded **in each direction** (prefix
    /// included). `None` leaves it uncapped; a public relay sets it.
    pub byte_quota: Option<u64>,
    /// Optional cap on total frames forwarded **in each direction**. `None`
    /// leaves it uncapped.
    pub frame_quota: Option<u64>,
}

impl Default for SpliceLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_RELAY_FRAME_BYTES,
            byte_quota: None,
            frame_quota: None,
        }
    }
}

/// Why a splice ended. Everything but [`Closed`](Self::Closed) is a fail-closed
/// teardown; even a clean close of one direction closes the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpliceOutcome {
    /// One endpoint closed cleanly at a frame boundary; the relay then closed
    /// the peer. The named direction is the one that reached end-of-stream.
    Closed(Direction),
    /// A frame's declared length exceeded [`SpliceLimits::max_frame_bytes`].
    /// Rejected from the length prefix alone, before allocation.
    FrameOversize(Direction),
    /// A per-direction byte or frame quota was reached.
    QuotaExceeded(Direction),
    /// The controller stopped the splice (revoke/stop) or was dropped.
    Cancelled,
    /// A transport I/O error or a truncated frame (an endpoint vanished
    /// mid-frame). The outcome is unknown, never a fabricated success.
    Failed(Direction),
}

/// Metadata-only record of a finished splice. It carries counts and a terminal
/// outcome, never any forwarded bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpliceReport {
    /// Frames forwarded from `a` to `b`.
    pub a_to_b_frames: u64,
    /// Wire bytes (prefixes included) forwarded from `a` to `b`.
    pub a_to_b_bytes: u64,
    /// Frames forwarded from `b` to `a`.
    pub b_to_a_frames: u64,
    /// Wire bytes (prefixes included) forwarded from `b` to `a`.
    pub b_to_a_bytes: u64,
    /// Why the splice ended.
    pub outcome: SpliceOutcome,
}

/// Per-direction counters. Each pump touches only its own [`Counters`], but the
/// splice future is spawned on the runtime, so the counters are atomics to keep
/// it `Send`; `Relaxed` suffices as there is no cross-counter ordering.
#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    bytes: AtomicU64,
}

/// Splices two connected streams into one opaque session pipe until one side
/// closes, a quota trips, a frame is oversize, the transport fails, or the
/// controller stops it.
///
/// The streams are the relay's two legs of a single session. On return, both
/// legs are dropped, so whichever endpoint was still open observes a close —
/// the relay never leaves a half-open session behind.
pub async fn splice<A, B>(a: A, b: B, limits: SpliceLimits, mut stop: StopHandle) -> SpliceReport
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);

    let ab = Counters::default();
    let ba = Counters::default();

    let outcome = {
        let ab_pump = pump(&mut ar, &mut bw, Direction::AtoB, &limits, &ab);
        let ba_pump = pump(&mut br, &mut aw, Direction::BtoA, &limits, &ba);
        tokio::pin!(ab_pump, ba_pump);
        tokio::select! {
            o = &mut ab_pump => o,
            o = &mut ba_pump => o,
            () = stop.stopped() => SpliceOutcome::Cancelled,
        }
    };

    // Dropping the halves here closes both legs: the still-open endpoint sees a
    // close, not a fabricated clean result (REQ-N-008).
    SpliceReport {
        a_to_b_frames: ab.frames.load(Ordering::Relaxed),
        a_to_b_bytes: ab.bytes.load(Ordering::Relaxed),
        b_to_a_frames: ba.frames.load(Ordering::Relaxed),
        b_to_a_bytes: ba.bytes.load(Ordering::Relaxed),
        outcome,
    }
}

/// Forwards frames one at a time from `reader` to `writer`, enforcing the size
/// ceiling and quotas and keeping metadata counts. Returns the terminal outcome
/// for this direction; the caller tears the peer down.
async fn pump<R, W>(
    reader: &mut R,
    writer: &mut W,
    dir: Direction,
    limits: &SpliceLimits,
    counters: &Counters,
) -> SpliceOutcome
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let mut prefix = [0u8; LENGTH_PREFIX_LEN];
        match fill(reader, &mut prefix).await {
            Fill::Ok => {}
            // A clean end-of-stream exactly at a frame boundary is the normal
            // way a session ends.
            Fill::CleanEof => return SpliceOutcome::Closed(dir),
            // A partial prefix or an I/O error is a broken leg, not a clean end.
            Fill::Truncated | Fill::Io => return SpliceOutcome::Failed(dir),
        }

        let len = u32::from_le_bytes(prefix) as usize;
        if len > limits.max_frame_bytes {
            return SpliceOutcome::FrameOversize(dir);
        }

        let wire = (LENGTH_PREFIX_LEN as u64).saturating_add(len as u64);
        if let Some(frame_quota) = limits.frame_quota
            && counters.frames.load(Ordering::Relaxed).saturating_add(1) > frame_quota
        {
            return SpliceOutcome::QuotaExceeded(dir);
        }
        if let Some(byte_quota) = limits.byte_quota
            && counters.bytes.load(Ordering::Relaxed).saturating_add(wire) > byte_quota
        {
            return SpliceOutcome::QuotaExceeded(dir);
        }

        let mut body = vec![0u8; len];
        // A zero-length frame is valid framing: `fill` returns `Ok` without a
        // read. A short read here means the endpoint vanished mid-frame.
        match fill(reader, &mut body).await {
            Fill::Ok => {}
            Fill::CleanEof | Fill::Truncated | Fill::Io => return SpliceOutcome::Failed(dir),
        }

        if writer.write_all(&prefix).await.is_err()
            || writer.write_all(&body).await.is_err()
            || writer.flush().await.is_err()
        {
            return SpliceOutcome::Failed(dir);
        }

        counters.frames.fetch_add(1, Ordering::Relaxed);
        counters.bytes.fetch_add(wire, Ordering::Relaxed);
    }
}

/// The result of trying to fill a buffer completely.
enum Fill {
    /// The buffer was filled.
    Ok,
    /// End-of-stream with no bytes read: a clean close at a boundary.
    CleanEof,
    /// End-of-stream after a partial read: a truncated frame.
    Truncated,
    /// An I/O error.
    Io,
}

/// Reads until `buf` is full, distinguishing a clean boundary close from a
/// truncated frame so the caller can report an honest outcome. `read_exact`
/// cannot make that distinction, so this is spelled out.
async fn fill<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut [u8]) -> Fill {
    let mut read = 0;
    while read < buf.len() {
        match reader.read(&mut buf[read..]).await {
            Ok(0) => {
                return if read == 0 {
                    Fill::CleanEof
                } else {
                    Fill::Truncated
                };
            }
            Ok(n) => read += n,
            Err(_) => return Fill::Io,
        }
    }
    Fill::Ok
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::cancel::StopSignal;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    /// Writes one length-prefixed frame with the given opaque body.
    async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) {
        let len = u32::try_from(body.len()).unwrap();
        w.write_all(&len.to_le_bytes()).await.unwrap();
        w.write_all(body).await.unwrap();
        w.flush().await.unwrap();
    }

    /// Reads one length-prefixed frame body.
    async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Vec<u8> {
        let mut prefix = [0u8; 4];
        r.read_exact(&mut prefix).await.unwrap();
        let len = u32::from_le_bytes(prefix) as usize;
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await.unwrap();
        body
    }

    // Opaque frames pass through unchanged in both directions, and the report
    // counts the wire bytes and frames (metadata only).
    #[tokio::test]
    async fn forwards_frames_both_ways_and_counts_them() {
        let (a_end, relay_a) = duplex(64 * 1024);
        let (relay_b, b_end) = duplex(64 * 1024);
        let (signal, stop) = StopSignal::new();

        let task = tokio::spawn(splice(relay_a, relay_b, SpliceLimits::default(), stop));

        let mut a_end = a_end;
        let mut b_end = b_end;
        // a -> b
        write_frame(&mut a_end, b"hello-from-a").await;
        assert_eq!(read_frame(&mut b_end).await, b"hello-from-a");
        // b -> a
        write_frame(&mut b_end, b"reply-from-b").await;
        assert_eq!(read_frame(&mut a_end).await, b"reply-from-b");

        // Clean close from a ends the session.
        drop(a_end);
        let report = task.await.unwrap();

        assert_eq!(report.a_to_b_frames, 1);
        assert_eq!(report.a_to_b_bytes, 4 + 12);
        assert_eq!(report.b_to_a_frames, 1);
        assert_eq!(report.b_to_a_bytes, 4 + 12);
        assert!(matches!(
            report.outcome,
            SpliceOutcome::Closed(Direction::AtoB)
        ));
        drop(signal);
    }

    // A declared length over the ceiling is rejected from the prefix alone,
    // before the body is allocated or read — fail closed (REQ-N-012).
    #[tokio::test]
    async fn oversize_prefix_fails_closed_before_alloc() {
        let (a_end, relay_a) = duplex(64);
        let (relay_b, _b_end) = duplex(64);
        let limits = SpliceLimits {
            max_frame_bytes: 8,
            ..SpliceLimits::default()
        };
        let (_signal, stop) = StopSignal::new();
        let task = tokio::spawn(splice(relay_a, relay_b, limits, stop));

        let mut a_end = a_end;
        // Declare 9 bytes (> 8) but send no body: if the pump allocated and
        // waited for the body it would hang here.
        a_end.write_all(&9u32.to_le_bytes()).await.unwrap();
        a_end.flush().await.unwrap();

        let report = task.await.unwrap();
        assert!(matches!(
            report.outcome,
            SpliceOutcome::FrameOversize(Direction::AtoB)
        ));
        assert_eq!(report.a_to_b_frames, 0);
    }

    // A per-direction byte quota stops forwarding fail-closed once crossing it,
    // and never forwards the frame that would exceed it.
    #[tokio::test]
    async fn byte_quota_stops_fail_closed() {
        let (a_end, relay_a) = duplex(64 * 1024);
        let (relay_b, b_end) = duplex(64 * 1024);
        let limits = SpliceLimits {
            byte_quota: Some(4 + 4), // exactly one 4-byte-body frame fits
            ..SpliceLimits::default()
        };
        let (_signal, stop) = StopSignal::new();
        let task = tokio::spawn(splice(relay_a, relay_b, limits, stop));

        let mut a_end = a_end;
        let mut b_end = b_end;
        write_frame(&mut a_end, b"aaaa").await; // within quota
        assert_eq!(read_frame(&mut b_end).await, b"aaaa");
        write_frame(&mut a_end, b"bbbb").await; // would exceed -> rejected

        let report = task.await.unwrap();
        assert!(matches!(
            report.outcome,
            SpliceOutcome::QuotaExceeded(Direction::AtoB)
        ));
        assert_eq!(report.a_to_b_frames, 1, "only the in-quota frame forwarded");
        // The over-quota frame never reached b: the next read closes, not data.
        let mut tail = Vec::new();
        let _ = b_end.read_to_end(&mut tail).await;
        assert!(tail.is_empty());
    }

    // Cancellation tears both directions down promptly and reports Cancelled,
    // and the still-open endpoint sees a close rather than a hang.
    #[tokio::test]
    async fn cancellation_fails_closed() {
        let (a_end, relay_a) = duplex(64 * 1024);
        let (relay_b, b_end) = duplex(64 * 1024);
        let (signal, stop) = StopSignal::new();
        let task = tokio::spawn(splice(relay_a, relay_b, SpliceLimits::default(), stop));

        // Idle session, then the controller stops it.
        signal.stop();
        let report = task.await.unwrap();
        assert!(matches!(report.outcome, SpliceOutcome::Cancelled));

        // Both legs are closed: reads at the endpoints reach EOF, not a hang.
        let mut a_end = a_end;
        let mut b_end = b_end;
        let mut buf = Vec::new();
        assert!(a_end.read_to_end(&mut buf).await.is_ok());
        assert!(b_end.read_to_end(&mut buf).await.is_ok());
    }

    // A leg that vanishes mid-frame is a broken outcome, not a clean close.
    #[tokio::test]
    async fn truncated_frame_fails_closed() {
        let (a_end, relay_a) = duplex(64 * 1024);
        let (relay_b, _b_end) = duplex(64 * 1024);
        let (_signal, stop) = StopSignal::new();
        let task = tokio::spawn(splice(relay_a, relay_b, SpliceLimits::default(), stop));

        let mut a_end = a_end;
        // Announce 10 bytes, send 3, then vanish.
        a_end.write_all(&10u32.to_le_bytes()).await.unwrap();
        a_end.write_all(b"abc").await.unwrap();
        a_end.flush().await.unwrap();
        drop(a_end);

        let report = task.await.unwrap();
        assert!(matches!(
            report.outcome,
            SpliceOutcome::Failed(Direction::AtoB)
        ));
    }
}
