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

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;

use cretlink_relay::{AdmissionToken, Preamble, PreambleError, RendezvousTicket, write_preamble};
use cretlink_wss::{WssClientStream, WssConnector, WssError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::SessionError;

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
    /// spawned session task. An unreachable target, a rejected preamble, or any
    /// other transport fault surfaces as [`SessionError::Io`] and never a
    /// fabricated stream (REQ-N-008, REQ-N-015).
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
    type Stream = TcpStream;

    async fn dial(&self) -> Result<Self::Stream, SessionError> {
        let mut stream = TcpStream::connect(self.relay)
            .await
            .map_err(SessionError::Io)?;
        // Clone the ticket and token into the one-shot preamble. Both are
        // zeroizing secrets that are `Clone` (not `Copy`), so the clones are
        // erased on drop alongside the originals; the relay consumes them to
        // admit and match this leg, and nothing after the preamble is anything
        // but the opaque Noise session (ADR-0016 §5).
        let preamble = Preamble::new(self.token.clone(), self.ticket.clone());
        write_preamble(&mut stream, &preamble)
            .await
            .map_err(map_preamble_error)?;
        Ok(stream)
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
    /// Fails closed as [`SessionError::Io`] if the pinned PEM holds no usable
    /// certificate, if the OS exposes no trust anchors, or if the TLS client
    /// configuration is rejected — never yielding a dialer with an empty or
    /// unusable trust store (REQ-S-020).
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
    /// Fails closed as [`SessionError::Io`] if the pinned PEM holds no usable
    /// certificate, if the OS exposes no trust anchors, or if the TLS client
    /// configuration is rejected (REQ-S-020).
    pub fn connector(pinned_ca_pem: Option<&[u8]>) -> Result<WssConnector, SessionError> {
        let roots = match pinned_ca_pem {
            Some(pem) => {
                let cas = cretlink_wss::certs_from_pem(pem).map_err(map_wss_error)?;
                cretlink_wss::pinned_roots(cas).map_err(map_wss_error)?
            }
            None => cretlink_wss::system_roots().map_err(map_wss_error)?,
        };
        let config = cretlink_wss::client_config(roots).map_err(map_wss_error)?;
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
    type Stream = WssClientStream<TcpStream>;

    async fn dial(&self) -> Result<Self::Stream, SessionError> {
        // Connect the raw TCP leg, then complete TLS + WebSocket. The relay's
        // certificate is validated against `server_name` inside `connect`; an
        // untrusted or mismatched certificate returns an error and no stream, so
        // the preamble below is never written on an unauthenticated channel.
        let tcp = TcpStream::connect(self.relay)
            .await
            .map_err(SessionError::Io)?;
        let mut stream = cretlink_wss::connect(&self.connector, &self.server_name, tcp)
            .await
            .map_err(map_wss_error)?;
        // From here the leg is identical to a raw relay dial: write the one-shot
        // rendezvous preamble, after which the relay only ever sees the opaque
        // Noise session (ADR-0016 §5). `write_preamble` flushes, so the preamble
        // is delivered as a single WebSocket binary message.
        let preamble = Preamble::new(self.token.clone(), self.ticket.clone());
        write_preamble(&mut stream, &preamble)
            .await
            .map_err(map_preamble_error)?;
        Ok(stream)
    }
}

/// Maps a preamble-write failure onto the session error surface so a relay dial
/// fails closed through the same [`SessionError::Io`] channel a direct dial uses.
///
/// A transport I/O failure keeps its underlying [`io::Error`]; a structural fault
/// (an over-long admission token) is reported as an `InvalidInput` I/O error
/// carrying the typed reason, so a caller retry loop that already distinguishes
/// [`SessionError::Io`] treats both uniformly (REQ-N-015).
fn map_preamble_error(err: PreambleError) -> SessionError {
    match err {
        PreambleError::Io(io) => SessionError::Io(io),
        other => SessionError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            other.to_string(),
        )),
    }
}

/// Maps a wss transport failure onto the session error surface. A socket-level
/// TLS I/O fault (including an untrusted or hostname-mismatched relay
/// certificate, which surfaces as [`WssError::Io`]) keeps its underlying
/// [`io::Error`]; every other setup fault (invalid server name, WebSocket
/// handshake, TLS configuration) is reported as an I/O error carrying the typed
/// reason, so a relay dial fails closed through the same [`SessionError::Io`]
/// channel as a direct dial (REQ-N-015, REQ-S-020).
fn map_wss_error(err: WssError) -> SessionError {
    match err {
        WssError::Io(io) => SessionError::Io(io),
        other => SessionError::Io(io::Error::other(other.to_string())),
    }
}
