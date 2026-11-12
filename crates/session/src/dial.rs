//! The transport dial abstraction (ST-M007-008, ADR-0016 §5).
//!
//! A CretLink session runs over exactly one duplex byte stream. On the LAN the
//! agent obtains that stream by connecting straight to the console's private
//! listener; over the internet relay it obtains the *same kind of* stream by
//! connecting outbound to the relay and writing a one-shot rendezvous preamble,
//! after which the relay splices it to the console's leg. In both cases what the
//! session runtime receives is an opaque, end-to-end-encrypted stream — the
//! [pairing](cretlink_transport::Pairing), the Session Approval Code, consent,
//! attribution, revocation, and cleanup are byte-identical (REQ-F-005,
//! REQ-F-016..019).
//!
//! [`Dial`] captures precisely that difference and nothing more: it is the only
//! seam between "LAN direct connect" and "relay rendezvous". Selecting the relay
//! is a **transport choice, not a session-logic change** (ADR-0007,
//! ADR-0016 §5) — the runtime is generic over the stream and never learns which
//! dialer produced it.
//!
//! The trait method returns an explicit `impl Future + Send` (not `async fn`) so
//! the future carries the `Send` bound the spawned session task needs, and so a
//! new transport slots in as one more implementor with no change here. The
//! wss-over-TLS dialer of ST-M007-009 ([`WssRelayDial`]) is exactly such an
//! implementor: it dials the relay over an authenticated TLS + WebSocket envelope
//! yet still yields the same opaque, end-to-end-encrypted byte stream, so the
//! session runtime remains unaware of the transport.
//!
//! # What a relay dial tells the caller (ST-M007-012)
//!
//! The relay answers the rendezvous preamble once ([`RelayAnswer`]). A relay
//! dial therefore fails with a reason a person can act on ([`RelayFailure`]):
//! the relay cannot be reached, the protected connection cannot be set up, the
//! relay does not answer, or the relay refuses and says why. An admitted leg
//! is handed out as a [`RelayLeg`]: the same bytes, plus one fact the session
//! runtime cannot know, namely that a stream which ends before the peer was
//! heard is a rendezvous that nobody joined ([`rendezvous_lapsed`]).
//! [`WaitingDial`] uses that fact to keep a rendezvous open for as long as the
//! technician waits.
//!

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use cretlink_relay::{
    AdmissionToken, Preamble, PreambleError, RelayAnswer, RelayRefusal, RendezvousTicket,
    read_answer, write_preamble,
};
use cretlink_wss::{WssClientStream, WssConnector, WssError};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::SessionError;

/// How long an endpoint waits for the relay's answer to its preamble.
pub const RELAY_ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`WaitingDial`] pauses before it dials again after a rendezvous
/// lapsed. Short, because the customer may be typing the code right now; not
/// zero, so that a relay which closes at once is not dialled in a tight loop
/// (its rate limit would then refuse, and that refusal ends the wait).
pub const DEFAULT_REDIAL_PAUSE: Duration = Duration::from_secs(1);

/// Why a session could not be set up through a relay (ST-M007-012,
/// ST-M007-013). Every reason is secret-free and is one a person can act on;
/// the application words them for the technician and the customer.
#[derive(Debug, Error)]
pub enum RelayFailure {
    /// No connection to the relay's address: it is down, the address is
    /// wrong, or this computer has no way out to it.
    #[error("the relay could not be reached")]
    Unreachable(#[source] io::Error),
    /// The protected connection to the relay could not be set up: its
    /// certificate is not trusted or does not carry its name, or the server
    /// on that address speaks no TLS. Nothing was sent to it.
    #[error("the protected connection to the relay could not be set up")]
    Untrusted(#[source] io::Error),
    /// The server did not answer the rendezvous preamble as a relay of this
    /// version does: an older relay server, or another service.
    #[error("the relay did not answer")]
    NoAnswer(#[source] io::Error),
    /// The relay refused this computer and said why.
    #[error("the relay refused the connection")]
    Refused(#[source] RelayRefusal),
    /// The relay settings cannot be used as they are, for example a name that
    /// is no host name or a certificate file without a certificate.
    #[error("the relay settings cannot be used: {0}")]
    Settings(String),
}

/// The marker inside the I/O error of a [`RelayLeg`] whose stream ended before
/// the peer was heard. Found again with [`rendezvous_lapsed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("nobody joined the rendezvous before the relay let it lapse")]
pub struct RendezvousLapsed;

/// Whether `error` says that a relay rendezvous lapsed: the relay admitted
/// this leg, and then closed it before a single byte of the peer arrived.
///
/// On the waiting side that means "not yet": the peer has not typed the code.
/// On the joining side it means that nobody waits on that code: it is wrong,
/// already used, or the other side stopped waiting.
///
/// The marker travels inside an [`io::Error`], possibly wrapped by the pairing
/// and session errors, so this walks the whole chain of sources.
#[must_use]
pub fn rendezvous_lapsed(error: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<RendezvousLapsed>() {
            return true;
        }
        // An `io::Error` does not list its payload as its source.
        if let Some(io) = error.downcast_ref::<io::Error>()
            && io
                .get_ref()
                .is_some_and(|payload| payload.is::<RendezvousLapsed>())
        {
            return true;
        }
        current = error.source();
    }
    false
}

/// The stream of a relay leg that the relay admitted (ST-M007-012).
///
/// It passes every byte through unchanged in both directions; the relay and
/// this type see the end-to-end session only as opaque bytes. Its one
/// addition: if the stream ends, or the connection is cut, before a single
/// byte of the peer arrived, the read fails with an error for which
/// [`rendezvous_lapsed`] is true, instead of an end of stream that looks like
/// a peer who hung up.
#[derive(Debug)]
pub struct RelayLeg<S> {
    inner: S,
    /// A byte of the peer that [`RelayLeg::wait_for_peer`] already took.
    first: Option<u8>,
    /// Whether a byte of the peer has arrived.
    heard: bool,
}

impl<S> RelayLeg<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            first: None,
            heard: false,
        }
    }
}

impl<S: AsyncRead + Unpin> RelayLeg<S> {
    /// Waits until the peer is heard, without consuming anything: the first
    /// byte is kept and read again by the session runtime.
    ///
    /// # Errors
    ///
    /// The I/O error of the stream. For a rendezvous that nobody joined
    /// [`rendezvous_lapsed`] is true of it.
    pub async fn wait_for_peer(&mut self) -> io::Result<()> {
        if self.heard {
            return Ok(());
        }
        let mut byte = [0u8; 1];
        // A read of this type: it turns an early end into the lapse error.
        let read = self.read(&mut byte).await?;
        if read == 1 {
            self.first = Some(byte[0]);
        }
        Ok(())
    }
}

/// Whether an I/O error before the peer was heard is the relay closing a
/// parked leg, as opposed to something worth reporting on its own. A relay
/// drops the connection without a farewell, which TLS reports as an
/// unexpected end.
fn is_cut(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

fn lapsed() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, RendezvousLapsed)
}

impl<S: AsyncRead + Unpin> AsyncRead for RelayLeg<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if let Some(byte) = this.first.take() {
            buf.put_slice(&[byte]);
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) if buf.filled().len() > before => {
                this.heard = true;
                Poll::Ready(Ok(()))
            }
            // The stream ended. Before the peer was heard, that is the relay
            // letting the rendezvous lapse.
            Poll::Ready(Ok(())) if !this.heard => Poll::Ready(Err(lapsed())),
            Poll::Ready(Err(error)) if !this.heard && is_cut(&error) => Poll::Ready(Err(lapsed())),
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for RelayLeg<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// A way to obtain the single duplex stream one session runs over.
///
/// The associated [`Stream`](Dial::Stream) is bounded exactly as the session
/// runtime requires (`AsyncRead + AsyncWrite + Unpin + Send + 'static`), so the
/// runtime is monomorphized over the concrete stream a dialer yields rather than
/// boxed behind a trait object. [`DirectDial`] yields a plain [`TcpStream`];
/// [`RelayDial`] yields the same `TcpStream` after writing its rendezvous
/// preamble.
pub trait Dial {
    /// The duplex stream this dialer produces.
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;

    /// Obtains the stream, or fails closed with a [`SessionError`].
    ///
    /// The returned future is `Send` so the caller can hand the stream to a
    /// spawned session task. An unreachable LAN target surfaces as
    /// [`SessionError::Io`] and a failed relay dial as [`SessionError::Relay`]
    /// with its reason; neither ever yields a fabricated stream (REQ-N-008,
    /// REQ-N-015).
    fn dial(&self) -> impl Future<Output = Result<Self::Stream, SessionError>> + Send;
}

/// A direct LAN dial: connect a TCP stream straight to the console's private
/// listener (ADR-0007). This is the historical, unchanged LAN path.
#[derive(Debug, Clone, Copy)]
pub struct DirectDial {
    /// The console's private-interface address to connect to.
    pub target: SocketAddr,
}

impl DirectDial {
    /// A direct dialer for `target` (the console's private listener address).
    #[must_use]
    pub fn new(target: SocketAddr) -> Self {
        Self { target }
    }
}

impl Dial for DirectDial {
    type Stream = TcpStream;

    async fn dial(&self) -> Result<Self::Stream, SessionError> {
        TcpStream::connect(self.target)
            .await
            .map_err(SessionError::Io)
    }
}

/// A relay rendezvous dial (ADR-0016 §5): connect **outbound** to the relay,
/// write the one-shot rendezvous preamble naming this leg's single-use
/// [`RendezvousTicket`] (plus an optional admission token), then hand the same
/// TCP stream to the session runtime. After the preamble the relay only ever
/// sees the opaque end-to-end Noise session; it is never a command authority or
/// a plaintext endpoint (REQ-F-022, REQ-S-005).
///
/// Both endpoints construct a `RelayDial` with the **same** ticket and both dial
/// the relay; the relay matches them on the ticket and splices. The console then
/// pairs as the Noise responder and the agent as the initiator, exactly as on
/// the LAN — only the transport differs.
///
pub struct RelayDial {
    /// The relay's reachable address; both endpoints dial it outbound.
    pub relay: SocketAddr,
    /// The single-use ticket naming this pending pairing. The relay splices the
    /// two legs that present the same ticket.
    pub ticket: RendezvousTicket,
    /// The optional shared admission credential — abuse control only, never
    /// session authority (ADR-0016 §4). `None` for an open relay.
    pub token: Option<AdmissionToken>,
}

impl RelayDial {
    /// A relay dialer for `relay`, presenting `ticket` and an optional admission
    /// `token`.
    #[must_use]
    pub fn new(relay: SocketAddr, ticket: RendezvousTicket, token: Option<AdmissionToken>) -> Self {
        Self {
            relay,
            ticket,
            token,
        }
    }
}

impl fmt::Debug for RelayDial {
    /// Shows the relay address (non-secret routing metadata) and redacts the
    /// ticket and the presence-only token flag: neither the single-use ticket nor
    /// the admission secret is ever printed (REQ-S-017).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayDial")
            .field("relay", &self.relay)
            .field("ticket", &"<redacted>")
            .field("has_token", &self.token.is_some())
            .finish()
    }
}

impl Dial for RelayDial {
    type Stream = RelayLeg<TcpStream>;

    async fn dial(&self) -> Result<Self::Stream, SessionError> {
        let stream = TcpStream::connect(self.relay)
            .await
            .map_err(RelayFailure::Unreachable)?;
        // Clone the ticket and token into the one-shot preamble. Both are
        // zeroizing secrets that are `Clone` (not `Copy`), so the clones are
        // erased on drop alongside the originals; the relay consumes them to
        // admit and match this leg, and nothing after the relay's answer is
        // anything but the opaque Noise session (ADR-0016 §5).
        let preamble = Preamble::new(self.token.clone(), self.ticket.clone());
        rendezvous(stream, &preamble).await
    }
}

/// Writes the rendezvous preamble on a connected relay stream and reads the
/// relay's answer, the same way over raw TCP and over wss. Only an admitted
/// leg comes back; everything else is a [`RelayFailure`] and no stream.
async fn rendezvous<S>(mut stream: S, preamble: &Preamble) -> Result<RelayLeg<S>, SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_preamble(&mut stream, preamble)
        .await
        .map_err(map_preamble_error)?;
    let answer = tokio::time::timeout(RELAY_ANSWER_TIMEOUT, read_answer(&mut stream))
        .await
        .map_err(|_elapsed| {
            RelayFailure::NoAnswer(io::Error::new(
                io::ErrorKind::TimedOut,
                "no answer to the rendezvous preamble in time",
            ))
        })?
        .map_err(map_preamble_error)?;
    match answer {
        RelayAnswer::Admitted => Ok(RelayLeg::new(stream)),
        RelayAnswer::Refused(refusal) => Err(RelayFailure::Refused(refusal).into()),
    }
}

/// A wss-over-TLS relay rendezvous dial (ST-M007-009, ADR-0016 §3): identical
/// to [`RelayDial`] in every session-visible respect, but the outbound leg is
/// wrapped in an authenticated TLS session and a WebSocket before the rendezvous
/// preamble is written. The relay presents a certificate the endpoint validates
/// against its pinned or system trust store; an untrusted or hostname-mismatched
/// certificate fails the handshake here and **no stream is returned** — the
/// preamble, and therefore the rendezvous metadata, never leaves the endpoint
/// (fail closed, REQ-N-015, REQ-S-020).
///
/// The yielded [`WssClientStream`] is a byte stream exactly like the raw
/// [`TcpStream`] of [`RelayDial`], so the carried Noise session, pairing,
/// consent, attribution, and revocation are byte-identical — only a passive
/// network observer's view differs, since the rendezvous preamble and the
/// session's framing now ride inside TLS records (ADR-0016 §3).
///
/// The [`WssConnector`] (and thus the trust decision: operator-pinned CA, the OS
/// root store, or both) is built by the endpoint application and injected here;
/// this dialer makes no trust-anchor policy of its own.
///
pub struct WssRelayDial {
    /// The relay's reachable address; both endpoints dial it outbound.
    pub relay: SocketAddr,
    /// The DNS name the relay's certificate is validated against. A mismatch
    /// fails the TLS handshake closed (REQ-S-020).
    pub server_name: String,
    /// The TLS connector carrying the endpoint's trust anchors. Reused across
    /// dials; it holds no per-session secret.
    connector: WssConnector,
    /// The single-use ticket naming this pending pairing (as in [`RelayDial`]).
    pub ticket: RendezvousTicket,
    /// The optional shared admission credential — abuse control only, never
    /// session authority. `None` for an open relay.
    pub token: Option<AdmissionToken>,
}

impl WssRelayDial {
    /// A wss relay dialer for `relay`, validating the relay certificate against
    /// `server_name` using the trust anchors already baked into `connector`, and
    /// presenting `ticket` with an optional admission `token`.
    #[must_use]
    pub fn new(
        relay: SocketAddr,
        server_name: impl Into<String>,
        connector: WssConnector,
        ticket: RendezvousTicket,
        token: Option<AdmissionToken>,
    ) -> Self {
        Self {
            relay,
            server_name: server_name.into(),
            connector,
            ticket,
            token,
        }
    }

    /// Builds a wss relay dialer, assembling the trust store here from the
    /// endpoint's deployment choice: when `pinned_ca_pem` is `Some`, the relay is
    /// trusted **only** via the operator-distributed CA/certificate it carries;
    /// when `None`, the operating system's root store is used (system roots the
    /// default, an operator-pinned CA the opt-in — the story's trust model). This
    /// keeps the TLS backend behind the [`cretlink_wss`] seam so callers need no
    /// direct dependency on it.
    ///
    /// # Errors
    ///
    /// Fails closed as [`SessionError::Relay`] ([`RelayFailure::Settings`]) if
    /// the pinned PEM holds no usable certificate, if the OS exposes no trust
    /// anchors, or if the TLS client configuration is rejected — never
    /// yielding a dialer with an empty or unusable trust store (REQ-S-020).
    pub fn with_trust(
        relay: SocketAddr,
        server_name: impl Into<String>,
        pinned_ca_pem: Option<&[u8]>,
        ticket: RendezvousTicket,
        token: Option<AdmissionToken>,
    ) -> Result<Self, SessionError> {
        let connector = Self::connector(pinned_ca_pem)?;
        Ok(Self::new(relay, server_name, connector, ticket, token))
    }

    /// Builds just the reusable [`WssConnector`] (trust store + TLS client config)
    /// from the endpoint's deployment choice, the same way [`with_trust`] does.
    ///
    /// A retrying dialer builds this **once**, fail-closed — so a malformed pinned
    /// CA aborts immediately instead of being retried forever — and clones the
    /// cheap `Arc`-backed connector into a fresh [`WssRelayDial`] per attempt via
    /// [`new`](Self::new).
    ///
    /// [`with_trust`]: Self::with_trust
    ///
    /// # Errors
    ///
    /// Fails closed as [`SessionError::Relay`] ([`RelayFailure::Settings`]) if
    /// the pinned PEM holds no usable certificate, if the OS exposes no trust
    /// anchors, or if the TLS client configuration is rejected (REQ-S-020).
    pub fn connector(pinned_ca_pem: Option<&[u8]>) -> Result<WssConnector, SessionError> {
        let roots = match pinned_ca_pem {
            Some(pem) => {
                let cas = cretlink_wss::certs_from_pem(pem).map_err(map_trust_error)?;
                cretlink_wss::pinned_roots(cas).map_err(map_trust_error)?
            }
            None => cretlink_wss::system_roots().map_err(map_trust_error)?,
        };
        let config = cretlink_wss::client_config(roots).map_err(map_trust_error)?;
        Ok(cretlink_wss::connector(config))
    }
}

impl fmt::Debug for WssRelayDial {
    /// Shows the relay address and validated server name (non-secret routing
    /// metadata) and redacts the ticket and the presence-only token flag, matching
    /// [`RelayDial`]'s redaction (REQ-S-017).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WssRelayDial")
            .field("relay", &self.relay)
            .field("server_name", &self.server_name)
            .field("ticket", &"<redacted>")
            .field("has_token", &self.token.is_some())
            .finish()
    }
}

impl Dial for WssRelayDial {
    type Stream = RelayLeg<WssClientStream<TcpStream>>;

    async fn dial(&self) -> Result<Self::Stream, SessionError> {
        // Connect the raw TCP leg, then complete TLS + WebSocket. The relay's
        // certificate is validated against `server_name` inside `connect`; an
        // untrusted or mismatched certificate returns an error and no stream, so
        // the preamble below is never written on an unauthenticated channel.
        let tcp = TcpStream::connect(self.relay)
            .await
            .map_err(RelayFailure::Unreachable)?;
        let stream = cretlink_wss::connect(&self.connector, &self.server_name, tcp)
            .await
            .map_err(map_wss_error)?;
        // From here the leg is identical to a raw relay dial: write the one-shot
        // rendezvous preamble and read the relay's answer, after which the relay
        // only ever sees the opaque Noise session (ADR-0016 §5).
        // `write_preamble` flushes, so the preamble is delivered as a single
        // WebSocket binary message.
        let preamble = Preamble::new(self.token.clone(), self.ticket.clone());
        rendezvous(stream, &preamble).await
    }
}

/// A relay dial that keeps the rendezvous open while the peer is not there
/// yet (ST-M007-012).
///
/// A relay parks a leg only for a short while and then closes it. The
/// technician may have to wait minutes while the customer finds the program
/// and types the code, so this dialer dials again with the same ticket every
/// time a rendezvous lapses, and returns only when the peer is heard. A
/// refusal, an unreachable relay or any other [`RelayFailure`] ends the wait
/// at once with its reason: only "nobody joined yet" is waited out.
///
/// It waits for as long as its future is polled. The caller ends the wait by
/// dropping the future, which closes the connection; the relay then forgets
/// the parked leg, and the code no longer finds anybody.
///
/// Use it on the side that waits, the technician's. The side that joins dials
/// once: a lapsed rendezvous there means that nobody waits on that code.
#[derive(Debug)]
pub struct WaitingDial<D> {
    inner: D,
    pause: Duration,
}

impl<D> WaitingDial<D> {
    /// Keeps the rendezvous of `inner` open, with [`DEFAULT_REDIAL_PAUSE`]
    /// between two dials.
    #[must_use]
    pub fn new(inner: D) -> Self {
        Self::with_pause(inner, DEFAULT_REDIAL_PAUSE)
    }

    /// Like [`WaitingDial::new`], with another pause between two dials.
    #[must_use]
    pub fn with_pause(inner: D, pause: Duration) -> Self {
        Self { inner, pause }
    }
}

impl<D, S> Dial for WaitingDial<D>
where
    D: Dial<Stream = RelayLeg<S>> + Sync,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RelayLeg<S>;

    async fn dial(&self) -> Result<Self::Stream, SessionError> {
        loop {
            let mut leg = self.inner.dial().await?;
            match leg.wait_for_peer().await {
                Ok(()) => return Ok(leg),
                // Nobody joined before the relay let the rendezvous lapse:
                // not a failure, the peer is simply not there yet.
                Err(error) if rendezvous_lapsed(&error) => {
                    drop(leg);
                    tokio::time::sleep(self.pause).await;
                }
                Err(error) => return Err(RelayFailure::Unreachable(error).into()),
            }
        }
    }
}

/// Maps a failure of the rendezvous preamble or of the relay's answer onto
/// [`RelayFailure`]. A connection that fails or ends is a relay that did not
/// answer, and so is an answer that is none of a relay of this version; an
/// admission key that is too long for the preamble is a matter of the
/// settings (REQ-N-015).
fn map_preamble_error(err: PreambleError) -> SessionError {
    let failure = match err {
        PreambleError::Io(io) => RelayFailure::NoAnswer(io),
        PreambleError::TokenTooLong { .. } => RelayFailure::Settings(err.to_string()),
        other => RelayFailure::NoAnswer(io::Error::new(
            io::ErrorKind::InvalidData,
            other.to_string(),
        )),
    };
    failure.into()
}

/// Maps a failure to connect through wss onto [`RelayFailure`]. A TLS
/// handshake that fails — which is how an untrusted certificate or one for
/// another name shows — is [`RelayFailure::Untrusted`]; a server that speaks
/// TLS but no WebSocket did not answer as a relay; a name that is no host
/// name is a matter of the settings (REQ-N-015, REQ-S-020).
fn map_wss_error(err: WssError) -> SessionError {
    let failure = match err {
        WssError::Io(io) => RelayFailure::Untrusted(io),
        WssError::Handshake(error) => RelayFailure::NoAnswer(io::Error::other(error.to_string())),
        other => RelayFailure::Settings(other.to_string()),
    };
    failure.into()
}

/// Maps a failure to assemble the trust anchors or the TLS client
/// configuration onto [`RelayFailure::Settings`]: nothing was dialled yet.
fn map_trust_error(err: WssError) -> SessionError {
    RelayFailure::Settings(err.to_string()).into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::VecDeque;

    use cretlink_transport::PairingError;
    use tokio::io::AsyncWriteExt;

    use super::*;

    /// A stream whose reads follow a script, for the ends and cuts that an
    /// in-memory pipe does not produce.
    struct Scripted(VecDeque<io::Result<Vec<u8>>>);

    impl Scripted {
        fn new(steps: impl IntoIterator<Item = io::Result<Vec<u8>>>) -> Self {
            Self(steps.into_iter().collect())
        }
    }

    impl AsyncRead for Scripted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.0.pop_front() {
                Some(Ok(bytes)) => {
                    buf.put_slice(&bytes);
                    Poll::Ready(Ok(()))
                }
                Some(Err(error)) => Poll::Ready(Err(error)),
                // The script is over: the stream ended.
                None => Poll::Ready(Ok(())),
            }
        }
    }

    fn cut(kind: io::ErrorKind) -> io::Result<Vec<u8>> {
        Err(io::Error::new(kind, "the connection was cut"))
    }

    #[tokio::test]
    async fn bytes_pass_through_unchanged_and_the_first_one_is_not_lost() {
        let (near, mut far) = tokio::io::duplex(64);
        let mut leg = RelayLeg::new(near);

        far.write_all(b"hello").await.unwrap();
        // Waiting for the peer takes nothing away from the session.
        leg.wait_for_peer().await.unwrap();
        leg.wait_for_peer().await.unwrap();
        let mut heard = [0u8; 5];
        leg.read_exact(&mut heard).await.unwrap();
        assert_eq!(&heard, b"hello");

        leg.write_all(b"ok").await.unwrap();
        leg.flush().await.unwrap();
        let mut answer = [0u8; 2];
        far.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"ok");
    }

    #[tokio::test]
    async fn an_end_before_the_peer_was_heard_is_a_lapsed_rendezvous() {
        // A plain read, as the session runtime does it.
        let (near, far) = tokio::io::duplex(64);
        drop(far);
        let mut leg = RelayLeg::new(near);
        let error = leg.read(&mut [0u8; 8]).await.unwrap_err();
        assert!(rendezvous_lapsed(&error), "{error:?}");

        // And the wait of the waiting side.
        let (near, far) = tokio::io::duplex(64);
        drop(far);
        let error = RelayLeg::new(near).wait_for_peer().await.unwrap_err();
        assert!(rendezvous_lapsed(&error), "{error:?}");
    }

    #[tokio::test]
    async fn an_end_after_the_peer_was_heard_is_an_ordinary_end() {
        let (near, mut far) = tokio::io::duplex(64);
        let mut leg = RelayLeg::new(near);
        far.write_all(b"x").await.unwrap();
        drop(far);

        let mut buf = [0u8; 8];
        assert_eq!(leg.read(&mut buf).await.unwrap(), 1);
        // The peer hung up: an end of stream, as on any connection.
        assert_eq!(leg.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_cut_counts_as_a_lapse_only_before_the_peer_was_heard() {
        for kind in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::BrokenPipe,
        ] {
            // A relay drops a parked leg without a farewell.
            let mut leg = RelayLeg::new(Scripted::new([cut(kind)]));
            let error = leg.read(&mut [0u8; 8]).await.unwrap_err();
            assert!(rendezvous_lapsed(&error), "{kind:?}");

            // Once the peer was heard the same cut is a lost connection.
            let mut leg = RelayLeg::new(Scripted::new([Ok(vec![1]), cut(kind)]));
            let mut buf = [0u8; 8];
            assert_eq!(leg.read(&mut buf).await.unwrap(), 1);
            let error = leg.read(&mut buf).await.unwrap_err();
            assert_eq!(error.kind(), kind);
            assert!(!rendezvous_lapsed(&error), "{kind:?}");
        }

        // Another kind of error is reported as it is, also before the peer.
        let mut leg = RelayLeg::new(Scripted::new([cut(io::ErrorKind::PermissionDenied)]));
        let error = leg.read(&mut [0u8; 8]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!rendezvous_lapsed(&error));
    }

    #[tokio::test]
    async fn a_read_into_an_empty_buffer_decides_nothing() {
        let (near, far) = tokio::io::duplex(64);
        drop(far);
        let mut leg = RelayLeg::new(near);
        assert_eq!(leg.read(&mut [0u8; 0]).await.unwrap(), 0);
        // The lapse is still found by the next real read.
        let error = leg.read(&mut [0u8; 1]).await.unwrap_err();
        assert!(rendezvous_lapsed(&error));
    }

    #[test]
    fn the_lapse_is_found_through_the_errors_that_wrap_it() {
        assert!(rendezvous_lapsed(&lapsed()));
        assert!(rendezvous_lapsed(&PairingError::Io(lapsed())));
        assert!(rendezvous_lapsed(&SessionError::Pairing(PairingError::Io(
            lapsed()
        ))));
        assert!(rendezvous_lapsed(&SessionError::Io(lapsed())));

        // Nothing else is a lapse.
        assert!(!rendezvous_lapsed(&io::Error::other("something else")));
        assert!(!rendezvous_lapsed(&PairingError::Eof));
        assert!(!rendezvous_lapsed(&SessionError::Disconnected));
        let refused = SessionError::Relay(RelayFailure::Refused(RelayRefusal::CredentialRejected));
        assert!(!rendezvous_lapsed(&refused));
        let unreachable = SessionError::Relay(RelayFailure::Unreachable(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "nothing listens there",
        )));
        assert!(!rendezvous_lapsed(&unreachable));
    }

    #[test]
    fn a_relay_failure_names_its_reason_in_its_source() {
        let refused = SessionError::Relay(RelayFailure::Refused(RelayRefusal::RateLimited));
        let reason = StdError::source(&refused)
            .and_then(StdError::source)
            .expect("the refusal is the source of the failure")
            .to_string();
        assert!(reason.contains("too many attempts"), "{reason}");
    }
}
