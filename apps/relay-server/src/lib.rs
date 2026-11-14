//! The CretLink internet relay server (ST-M007-007, ADR-0016 §5).
//!
//! This crate is the thin, portable-core-composing binary that turns the
//! transport-agnostic [`cretlink_relay`] leaf into a runnable TCP forwarder. It
//! adds exactly one thing the core deliberately leaves out — a real listener and
//! the per-connection glue — and **no** new trust logic:
//!
//! 1. accept an outbound connection and read its
//!    [rendezvous preamble](cretlink_relay::preamble) under a hard timeout,
//!    rejecting a malformed or oversized preamble *before* any forwarding;
//! 2. [`AdmissionControl::admit`] it — authentication (abuse control only, never
//!    session authority) → rate limit → concurrency quota — sourcing its posture
//!    from [`ControlConfig::public_default`](cretlink_relay::ControlConfig::public_default)
//!    — and **answer** the endpoint once ([`RelayAnswer`], ST-M007-012):
//!    admitted, or refused with the reason, so that an endpoint can tell a
//!    refusal from a rendezvous that nobody joined;
//! 3. [`Rendezvous::join`] on the single-use ticket, parking the first leg until
//!    its peer arrives;
//! 4. [`splice`](cretlink_relay::splice()) the matched pair under the admitted
//!    session's [`SpliceLimits`](cretlink_relay::SpliceLimits); and
//! 5. [`SessionGuard::finish`] each leg with the honest forwarding totals.
//!
//! # What it still is *not*
//!
//! The relay never becomes a command authority or a plaintext endpoint
//! (REQ-F-022, REQ-S-005). Everything after the preamble is the endpoints'
//! unmodified end-to-end Noise session (ADR-0010), which this process only ever
//! sees as opaque, length-bounded frames. It holds no session key, decodes no
//! [`cretlink_protocol`](cretlink_relay) message, and fabricates no clean
//! outcome: any fault — a bad preamble, a denied admission, a lone arrival, a
//! quota, an I/O error — fails closed and is recorded honestly (REQ-N-008).
//!
//! [`serve_ws`] is `serve_wss` without the TLS: for a relay behind a reverse
//! proxy that ends TLS (ST-M007-015).
//!
//! [`serve`] and [`serve_wss`] are the two transports over the **same** core:
//! [`serve`] forwards raw TCP — the ADR-0016 §5 documented interim, safe because
//! the relay is opaque end to end — and [`serve_wss`] wraps each hop in
//! WebSocket-over-TLS (the ADR-0016 §3 shipped default, ST-M007-009), terminating
//! TLS and upgrading to a WebSocket *before* the identical preamble → admit →
//! rendezvous → splice pipeline runs. Both are generic over the accepted stream
//! (`AsyncRead + AsyncWrite + Unpin + Send`), so no rendezvous, admission, or
//! metrics logic is duplicated per transport. The endpoint-side dialer that
//! *writes* the preamble is ST-M007-008/009; this server only ever reads it.
//!

pub mod keys;

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cretlink_relay::preamble::{MAX_PREAMBLE_TOKEN_BYTES, read_preamble};
use cretlink_relay::{
    AdmissionControl, Matched, PREAMBLE_VERSION, PreambleError, RelayAnswer, RelayRefusal,
    Rendezvous, SessionGuard, SpliceOutcome, SpliceReport, StopHandle, splice, write_answer,
};
use cretlink_wss::{WsByteStream, WssAcceptor, WssServerStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};

/// How long a fresh connection has to deliver a complete rendezvous preamble
/// before the relay drops it fail-closed. Bounds a stalled or slow-loris peer.
pub const DEFAULT_PREAMBLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a parked leg waits for its rendezvous peer before failing closed and
/// releasing its slot.
pub const DEFAULT_RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-connection timing and bounds for [`serve`]. [`Default`] is the documented
/// public posture.
#[derive(Debug, Clone, Copy)]
pub struct ServeConfig {
    /// Deadline for reading a complete rendezvous preamble.
    pub preamble_timeout: Duration,
    /// Deadline for a parked leg to be matched by its peer.
    pub rendezvous_timeout: Duration,
    /// Largest admission token accepted in the preamble, in bytes.
    pub max_token_bytes: usize,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            preamble_timeout: DEFAULT_PREAMBLE_TIMEOUT,
            rendezvous_timeout: DEFAULT_RENDEZVOUS_TIMEOUT,
            max_token_bytes: MAX_PREAMBLE_TOKEN_BYTES,
        }
    }
}

/// One admitted, parked-or-matched connection: the accepted carrier stream, the
/// concurrency [`SessionGuard`] that must outlive the splice, and the non-secret
/// peer label used only for metadata logging. The [`Rendezvous`] table moves whole
/// `Leg`s between arrivals; it never reads a byte of the stream.
///
/// `S` is the transport-specific carrier: a raw [`TcpStream`] under [`serve`], or
/// a [`WssServerStream`] under [`serve_wss`]. The rendezvous and splice are
/// identical either way — the transport is invisible below this type.
#[derive(Debug)]
pub struct Leg<S> {
    stream: S,
    guard: SessionGuard,
    peer: String,
}

/// Serves relay connections on `listener` until `stop` fires (or its controller
/// is dropped), composing the portable relay core for every connection.
///
/// `control` is the shared admission gate (clone it in; it is cheap to clone),
/// `rendezvous` the shared pending-pairing table, and `config` the per-connection
/// timing and bounds. Each accepted connection is handled on its own task, so one
/// slow or hostile peer never blocks another.
///
/// Returns when the stop signal fires; in-flight splices are torn down by the
/// same signal (each holds a clone), so shutdown fails closed rather than
/// abandoning a live forward.
pub async fn serve(
    listener: TcpListener,
    control: AdmissionControl,
    rendezvous: Arc<Rendezvous<Leg<TcpStream>>>,
    config: ServeConfig,
    stop: StopHandle,
) {
    // The raw-TCP transport upgrades nothing: the accepted stream *is* the carrier
    // the preamble and splice run over (ADR-0016 §5 documented interim).
    serve_loop(
        listener,
        control,
        rendezvous,
        config,
        stop,
        |tcp| async move { Ok::<TcpStream, io::Error>(tcp) },
    )
    .await;
}

/// Serves relay connections over **WebSocket-over-TLS** (`wss://`), the ADR-0016
/// §3 shipped default (ST-M007-009). Each accepted TCP connection is first
/// TLS-terminated with the operator's `acceptor` and upgraded to a WebSocket; the
/// resulting byte stream then feeds the **identical** preamble → admit →
/// rendezvous → splice pipeline as [`serve`]. The relay terminates only the
/// transport envelope — it never holds the end-to-end Noise session key or decodes
/// a protocol message (REQ-F-022, REQ-S-005).
///
/// The TLS + WebSocket handshake runs under the same
/// [`preamble_timeout`](ServeConfig::preamble_timeout) deadline as the preamble
/// read, so a peer that opens a connection but never completes the handshake
/// cannot park a slot (slow-loris).
pub async fn serve_wss(
    listener: TcpListener,
    acceptor: WssAcceptor,
    control: AdmissionControl,
    rendezvous: Arc<Rendezvous<Leg<WssServerStream<TcpStream>>>>,
    config: ServeConfig,
    stop: StopHandle,
) {
    serve_loop(listener, control, rendezvous, config, stop, move |tcp| {
        // Clone the acceptor into each connection's upgrade future; a `TlsAcceptor`
        // is an `Arc` inside, so this is cheap and keeps the closure `Fn`.
        let acceptor = acceptor.clone();
        async move {
            cretlink_wss::accept(&acceptor, tcp)
                .await
                .map_err(|err| io::Error::other(err.to_string()))
        }
    })
    .await;
}

/// Serves relay connections over **WebSocket without TLS**, for a relay behind
/// a reverse proxy that ends TLS in front of it (ST-M007-015). The endpoints
/// speak `wss` to the proxy as always; the proxy passes each WebSocket on to
/// this listener, and the **identical** preamble → admit → rendezvous → splice
/// pipeline runs over it.
///
/// The relay does not encrypt this hop. It must be one the operator trusts —
/// the same host or a private network — because the rendezvous preamble with
/// the admission key crosses it in the clear. What the endpoints say to each
/// other stays end-to-end encrypted and opaque to the proxy and the relay
/// alike (REQ-F-022, REQ-S-005).
///
/// Every connection comes from the proxy, so an open relay accounts all of
/// them under the proxy's address; with a key file each customer is accounted
/// under its label.
pub async fn serve_ws(
    listener: TcpListener,
    control: AdmissionControl,
    rendezvous: Arc<Rendezvous<Leg<WsByteStream<TcpStream>>>>,
    config: ServeConfig,
    stop: StopHandle,
) {
    serve_loop(
        listener,
        control,
        rendezvous,
        config,
        stop,
        |tcp| async move {
            cretlink_wss::accept_plain(tcp)
                .await
                .map_err(|err| io::Error::other(err.to_string()))
        },
    )
    .await;
}

/// The transport-generic accept loop shared by [`serve`] and [`serve_wss`]. It
/// accepts TCP connections until `stop` fires and hands each to
/// [`serve_connection`], which applies `upgrade` to turn the accepted [`TcpStream`]
/// into the carrier stream `S` (identity for raw TCP, TLS + WebSocket for wss)
/// before the identical rendezvous pipeline runs. No admission, rendezvous, or
/// metrics logic is duplicated per transport, so the two transports cannot drift.
async fn serve_loop<S, U, Fut>(
    listener: TcpListener,
    control: AdmissionControl,
    rendezvous: Arc<Rendezvous<Leg<S>>>,
    config: ServeConfig,
    mut stop: StopHandle,
    upgrade: U,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    U: Fn(TcpStream) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = io::Result<S>> + Send,
{
    // One monotonic clock for the whole server; admission and end events are
    // stamped as milliseconds since it started (no wall clock in the hot path).
    let clock = Instant::now();
    loop {
        let (stream, peer) = tokio::select! {
            () = stop.stopped() => break,
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(err) => {
                    // A transient accept error (e.g. a fd limit) must not kill
                    // the server; log metadata only and keep serving.
                    eprintln!("relay: accept failed: {err}");
                    continue;
                }
            },
        };
        let control = control.clone();
        let rendezvous = Arc::clone(&rendezvous);
        let conn_stop = stop.clone();
        let upgrade = upgrade.clone();
        tokio::spawn(async move {
            serve_connection(
                stream, peer, control, rendezvous, config, clock, conn_stop, upgrade,
            )
            .await;
        });
    }
}

/// Handles one accepted connection end to end: upgrade → preamble → admit →
/// rendezvous → splice → finish. Every failure path forwards zero bytes and
/// records honestly.
///
/// The parameters are the accepted socket plus the server context threaded from
/// [`serve_loop`] (admission, rendezvous, config, clock, stop) and the
/// transport `upgrade`; each is a distinct runtime value with no natural grouping.
#[allow(clippy::too_many_arguments)]
async fn serve_connection<S, U, Fut>(
    tcp: TcpStream,
    peer: SocketAddr,
    control: AdmissionControl,
    rendezvous: Arc<Rendezvous<Leg<S>>>,
    config: ServeConfig,
    clock: Instant,
    stop: StopHandle,
    upgrade: U,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    U: Fn(TcpStream) -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    // The accountability label is the source IP (not the ephemeral port), so
    // per-client rate limits and quotas group a client's connections. It is
    // non-secret metadata (REQ-S-017); the credential itself never lands here.
    let peer_label = peer.ip().to_string();

    // 0. Upgrade the accepted TCP into the carrier stream (identity for raw TCP,
    //    TLS + WebSocket accept for wss) under the preamble deadline, so a peer
    //    that opens a connection but never finishes the handshake cannot park a
    //    slot (slow-loris). A failed or timed-out upgrade forwards zero bytes.
    let mut stream = match tokio::time::timeout(config.preamble_timeout, upgrade(tcp)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            eprintln!("relay: rejected {peer_label}: transport upgrade failed: {err}");
            return;
        }
        Err(_elapsed) => {
            eprintln!(
                "relay: rejected {peer_label}: transport upgrade not completed within deadline"
            );
            return;
        }
    };

    // 1. Read the rendezvous preamble under a hard deadline. A malformed,
    //    oversized, or stalled preamble is rejected before anything is forwarded.
    let preamble = match tokio::time::timeout(
        config.preamble_timeout,
        read_preamble(&mut stream, config.max_token_bytes),
    )
    .await
    {
        Ok(Ok(preamble)) => preamble,
        Ok(Err(err)) => {
            eprintln!("relay: rejected {peer_label}: {err}");
            // An endpoint of another preamble version is told so: that status
            // keeps its meaning in every version. Anything else that is not a
            // well-formed preamble gets no answer.
            if matches!(err, PreambleError::UnsupportedVersion { .. }) {
                let refusal = RelayRefusal::UnsupportedVersion {
                    relay: PREAMBLE_VERSION,
                };
                if answer(&mut stream, RelayAnswer::Refused(refusal), &config).await {
                    discard_rest(&mut stream).await;
                }
            }
            return;
        }
        Err(_elapsed) => {
            eprintln!("relay: rejected {peer_label}: preamble not received within deadline");
            return;
        }
    };

    // 2. Admit: authentication → rate limit → concurrency quota. A denial forwards
    //    zero bytes; the gate has already recorded the metadata-only reason. The
    //    endpoint is told the reason, which names no credential and no session.
    let now_ms = ms_since(clock);
    let guard = match control.admit(preamble.token.as_ref(), &peer_label, now_ms) {
        Ok(guard) => guard,
        Err(denied) => {
            eprintln!("relay: denied {peer_label}: {denied}");
            answer(&mut stream, RelayAnswer::Refused(denied.into()), &config).await;
            return;
        }
    };

    // Who this leg is in the log: the label of its key, with the address it
    // came from. Never the key (REQ-S-017).
    let who = who(&guard, &peer_label);

    // A leg that would find neither its peer nor a place to park is told so,
    // instead of admitted and then dropped (ST-M007-014). Dropping the guard
    // records the admitted leg as ended with nothing forwarded.
    match rendezvous.has_room_for(&preamble.ticket) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("relay: no free place to park {who}");
            answer(
                &mut stream,
                RelayAnswer::Refused(RelayRefusal::Busy),
                &config,
            )
            .await;
            return;
        }
        Err(err) => {
            eprintln!("relay: cannot park {who}: {err}");
            let unavailable = RelayAnswer::Refused(RelayRefusal::Unavailable);
            answer(&mut stream, unavailable, &config).await;
            return;
        }
    }

    // The endpoint learns that it is admitted before the leg is parked. From
    // here on a stream that ends without a byte from the peer was not refused:
    // no session came about on this rendezvous. If the answer cannot be
    // written the leg is dropped: its guard records an honest unknown end.
    if !answer(&mut stream, RelayAnswer::Admitted, &config).await {
        eprintln!("relay: dropped {who}: the admission could not be answered");
        return;
    }

    // 3. Join the single-use rendezvous. The token is dropped here — it has done
    //    its only job (admission) and must not travel further.
    let ticket = preamble.ticket;
    drop(preamble.token);
    let leg = Leg {
        stream,
        guard,
        peer: who.clone(),
    };
    match rendezvous
        .join(ticket, leg, config.rendezvous_timeout)
        .await
    {
        Ok(Matched::Pair { first, second }) => {
            forward_pair(first, second, clock, stop).await;
        }
        Ok(Matched::Parked) => {
            // Our peer arrived and drives the splice, finishing both guards. This
            // side is done; the guard already moved into the matched pair.
        }
        Err(err) => {
            // Timeout / AtCapacity / Poisoned: `join` dropped our leg (and its
            // guard) fail-closed, so the abandoned session is recorded as an
            // honest unknown end. Nothing was forwarded.
            eprintln!("relay: rendezvous for {who} ended: {err}");
        }
    }
}

/// How a log line names an admitted leg: the label its key is accounted
/// under and the address it came from, or the address alone on a relay
/// without keys, where the address is the label.
fn who(guard: &SessionGuard, peer_label: &str) -> String {
    let client = guard.client().as_str();
    if client == peer_label {
        peer_label.to_string()
    } else {
        format!("{client} ({peer_label})")
    }
}

/// Writes the relay's one answer to an endpoint, under the preamble deadline so
/// that an endpoint which does not read cannot hold the connection open.
/// Returns whether the answer was written.
async fn answer<S>(stream: &mut S, answer: RelayAnswer, config: &ServeConfig) -> bool
where
    S: AsyncWrite + Unpin,
{
    matches!(
        tokio::time::timeout(config.preamble_timeout, write_answer(stream, answer)).await,
        Ok(Ok(()))
    )
}

/// How long the relay keeps reading after it refused a preamble of another
/// version, so that its answer arrives.
const DISCARD_TIMEOUT: Duration = Duration::from_millis(500);

/// Reads and discards the rest of a preamble that the relay refused after its
/// first bytes, bounded in bytes and in time.
///
/// The relay stops reading a preamble at the version byte when it does not
/// speak that version; the rest is still on its way. Closing a connection with
/// unread data makes the operating system reset it, and a reset can discard
/// the answer before the endpoint has read it. Reading what one preamble can
/// hold lets the connection close in order. Nothing that is read is used.
async fn discard_rest<S>(stream: &mut S)
where
    S: AsyncRead + Unpin,
{
    let mut left = MAX_PREAMBLE_TOKEN_BYTES + 64;
    let mut scratch = [0u8; 256];
    let discard = async {
        while left > 0 {
            let want = left.min(scratch.len());
            match stream.read(&mut scratch[..want]).await {
                Ok(0) | Err(_) => break,
                Ok(read) => left -= read,
            }
        }
    };
    let _ = tokio::time::timeout(DISCARD_TIMEOUT, discard).await;
}

/// Splices a matched pair under the admitted limits and finishes both guards.
///
/// The `second` (this-arrival) guard records the real per-direction totals from
/// the [`SpliceReport`]; the `first` (parked) guard records the *same* terminal
/// outcome with zero bytes, so the healthy path shows two honest ends rather than
/// one clean close and one fabricated-or-unknown end. The forwarded volume is
/// therefore counted exactly once.
async fn forward_pair<S>(first: Leg<S>, second: Leg<S>, clock: Instant, stop: StopHandle)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Leg {
        stream: first_stream,
        guard: first_guard,
        peer: first_peer,
    } = first;
    let Leg {
        stream: second_stream,
        guard: second_guard,
        peer: second_peer,
    } = second;

    let limits = second_guard.splice_limits();
    let report = splice(first_stream, second_stream, limits, stop).await;
    let end_ms = ms_since(clock);

    let bytes = report.a_to_b_bytes.saturating_add(report.b_to_a_bytes);
    let frames = report.a_to_b_frames.saturating_add(report.b_to_a_frames);
    eprintln!(
        "relay: session {first_peer}<->{second_peer} ended: {bytes} bytes, {frames} frames, {outcome:?}",
        outcome = report.outcome,
    );

    // The driving guard carries the true totals; the co-leg guard mirrors the
    // outcome with zeroed counts so the volume is not double-counted.
    second_guard.finish(end_ms, &report);
    first_guard.finish(end_ms, &coleg_report(report.outcome));
}

/// A zero-volume [`SpliceReport`] carrying only a terminal outcome, used to
/// [`finish`](SessionGuard::finish) the co-leg guard of a matched pair without
/// double-counting the bytes the driving guard already recorded.
fn coleg_report(outcome: SpliceOutcome) -> SpliceReport {
    SpliceReport {
        a_to_b_frames: 0,
        a_to_b_bytes: 0,
        b_to_a_frames: 0,
        b_to_a_bytes: 0,
        outcome,
    }
}

/// Milliseconds elapsed on the server's monotonic clock, saturating rather than
/// wrapping at the `u64` ceiling (an astronomically distant bound).
fn ms_since(clock: Instant) -> u64 {
    u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX)
}
