//! `cretlink-wss` — the wss-over-TLS transport envelope for the CretLink
//! internet relay (M007, ST-M007-009).
//!
//! The relay forwards an **opaque, end-to-end Noise session** and never decodes
//! it (ADR-0016 §5). This crate wraps that byte stream in `wss` — WebSocket over
//! TLS — so the metadata a passive network observer could otherwise read (the
//! rendezvous preamble, frame sizes, timing shape) is hidden inside a TLS record
//! stream that looks like ordinary web traffic, while the operator-terminated
//! relay still sees only ciphertext.
//!
//! It is deliberately thin and holds no session state or secrets:
//!
//! - [`server_config`] / [`client_config`] build the rustls configs (operator
//!   cert+key on the relay; a trust anchor on each endpoint);
//! - [`pinned_roots`] and [`system_roots`] assemble a client trust store from an
//!   operator-distributed certificate and/or the OS root store;
//! - [`accept`] / [`connect`] complete the TLS then WebSocket handshake and hand
//!   back a [`WsByteStream`] — an [`AsyncRead`] +
//!   [`AsyncWrite`] view of the connection, so the
//!   existing preamble reader, `cretlink_relay::Rendezvous`, and the session
//!   runtime operate over it exactly as over a bare `TcpStream`.
//! - [`accept_plain`] is the same without TLS, for a relay behind a reverse
//!   proxy that ends TLS (ST-M007-015).
//!
//! # Cryptographic backend
//!
//! [`rustls`] implements TLS in Rust, with `tokio-rustls` and `tokio-tungstenite`
//! on top. Its crypto provider is `aws-lc-rs` (ADR-0017): the widely audited
//! default provider, whose primitives come from AWS-LC (C and assembly).
//! `default-features = false` keeps `ring` and `openssl` out of the build.
//! CretLink invents no cryptography; it composes audited libraries. The config
//! builders are a single seam, so the provider can change without touching
//! callers.

#![forbid(unsafe_code)]

mod adapter;

#[cfg(feature = "test-cert")]
pub mod testcert;

use std::net::Ipv6Addr;
use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use rustls::pki_types::pem::{self, PemObject};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub use adapter::WsByteStream;

// Re-exports so callers assemble TLS material without depending on `rustls`
// directly — this crate is the single seam onto the TLS backend.
pub use rustls::RootCertStore as TrustStore;
pub use rustls::pki_types::{CertificateDer, PrivateKeyDer};
pub use tokio_rustls::{TlsAcceptor as WssAcceptor, TlsConnector as WssConnector};

/// The server-side byte stream [`accept`] yields over a transport `S`. The relay
/// runs its legs over `WssServerStream<TcpStream>`, so callers name the stream
/// type without depending on `tokio-rustls` directly.
pub type WssServerStream<S> = WsByteStream<tokio_rustls::server::TlsStream<S>>;

/// The client-side byte stream [`connect`] yields over a transport `S`. The
/// endpoint dialer runs its session over `WssClientStream<TcpStream>`.
pub type WssClientStream<S> = WsByteStream<tokio_rustls::client::TlsStream<S>>;

/// Errors from building the wss transport or completing its handshake.
///
/// Every variant is a fail-closed outcome: a malformed certificate, an
/// untrusted peer, or a botched handshake yields an error and no connection —
/// never a silently downgraded or unauthenticated stream.
#[derive(Debug, thiserror::Error)]
pub enum WssError {
    /// The TLS layer failed at the socket level (accept, connect, or transport).
    #[error("tls i/o failed: {0}")]
    Io(#[source] std::io::Error),
    /// The WebSocket handshake over the established TLS session failed. Boxed
    /// because the tungstenite error is large and would otherwise bloat every
    /// `Result` in this crate (`clippy::result_large_err`).
    #[error("websocket handshake failed: {0}")]
    Handshake(#[source] Box<tokio_tungstenite::tungstenite::Error>),
    /// A rustls configuration was rejected (e.g. a key that does not match the
    /// certificate, or an unusable trust anchor). Boxed for the same reason as
    /// [`WssError::Handshake`].
    #[error("tls configuration is invalid: {0}")]
    Tls(#[source] Box<rustls::Error>),
    /// The supplied server name is not a valid DNS name or IP address.
    #[error("invalid TLS server name: {0}")]
    InvalidServerName(String),
    /// The supplied PEM material could not be parsed.
    #[error("could not parse PEM material: {0}")]
    Pem(String),
    /// The PEM contained no certificate.
    #[error("no certificate found in the supplied PEM")]
    NoCertificates,
    /// The PEM contained no private key.
    #[error("no private key found in the supplied PEM")]
    NoPrivateKey,
    /// The operating system exposed no usable trust anchors.
    #[cfg(feature = "system-roots")]
    #[error("no usable system trust anchors were found")]
    NoSystemRoots,
    /// Test-only self-signed certificate generation failed.
    #[cfg(feature = "test-cert")]
    #[error("test certificate generation failed: {0}")]
    TestCert(String),
}

/// The `aws-lc-rs` crypto provider backing every config this crate builds
/// (ADR-0017).
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Builds the relay's server TLS config from its operator-supplied certificate
/// chain and private key. Client authentication is not required at the TLS layer
/// — endpoints authenticate each other end-to-end inside the carried Noise
/// session, and the relay stays a blind forwarder.
///
/// # Errors
///
/// Returns [`WssError::Tls`] if the protocol versions cannot be negotiated or
/// the private key does not match the certificate.
pub fn server_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>, WssError> {
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| WssError::Tls(Box::new(e)))?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| WssError::Tls(Box::new(e)))?;
    Ok(Arc::new(config))
}

/// Builds an endpoint's client TLS config that trusts exactly the anchors in
/// `roots` (from [`pinned_roots`], [`system_roots`], or both merged).
///
/// # Errors
///
/// Returns [`WssError::Tls`] if the protocol versions cannot be negotiated.
pub fn client_config(roots: RootCertStore) -> Result<Arc<ClientConfig>, WssError> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| WssError::Tls(Box::new(e)))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Assembles a trust store from operator-distributed certificates — the primary
/// mechanism for a self-hosted relay, where the operator hands its certificate
/// (or signing CA) to the technician console and the support agent to pin.
///
/// # Errors
///
/// Returns [`WssError::Tls`] if an anchor is unusable, or
/// [`WssError::NoCertificates`] if `cas` is empty.
pub fn pinned_roots(cas: Vec<CertificateDer<'static>>) -> Result<RootCertStore, WssError> {
    let mut roots = RootCertStore::empty();
    for ca in cas {
        roots.add(ca).map_err(|e| WssError::Tls(Box::new(e)))?;
    }
    if roots.is_empty() {
        return Err(WssError::NoCertificates);
    }
    Ok(roots)
}

/// Loads the operating system's native trust anchors, for a relay presenting a
/// certificate issued by a public CA rather than a pinned one. Individual
/// malformed anchors in the OS store are skipped so one bad entry cannot deny
/// the whole set.
///
/// # Errors
///
/// Returns [`WssError::NoSystemRoots`] if the OS exposes no usable anchors.
#[cfg(feature = "system-roots")]
pub fn system_roots() -> Result<RootCertStore, WssError> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        // A corrupt anchor must not deny the entire store; skip it.
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(WssError::NoSystemRoots);
    }
    Ok(roots)
}

/// Parses one or more certificates from PEM bytes (no file IO), for loading an
/// operator certificate chain or a pinned CA.
///
/// # Errors
///
/// Returns [`WssError::Pem`] on malformed PEM, or [`WssError::NoCertificates`]
/// if the input holds no certificate.
pub fn certs_from_pem(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, WssError> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| WssError::Pem(e.to_string()))?;
    if certs.is_empty() {
        return Err(WssError::NoCertificates);
    }
    Ok(certs)
}

/// Parses the first private key from PEM bytes (PKCS#8, PKCS#1, or SEC1).
///
/// # Errors
///
/// Returns [`WssError::Pem`] on malformed PEM, or [`WssError::NoPrivateKey`] if
/// the input holds no private key.
pub fn private_key_from_pem(pem: &[u8]) -> Result<PrivateKeyDer<'static>, WssError> {
    match PrivateKeyDer::from_pem_slice(pem) {
        Ok(key) => Ok(key),
        Err(pem::Error::NoItemsFound) => Err(WssError::NoPrivateKey),
        Err(e) => Err(WssError::Pem(e.to_string())),
    }
}

/// Wraps a server TLS config into a reusable [`TlsAcceptor`].
#[must_use]
pub fn acceptor(config: Arc<ServerConfig>) -> TlsAcceptor {
    TlsAcceptor::from(config)
}

/// Wraps a client TLS config into a reusable [`TlsConnector`].
#[must_use]
pub fn connector(config: Arc<ClientConfig>) -> TlsConnector {
    TlsConnector::from(config)
}

/// Completes the server side of the envelope over an accepted transport `tcp`:
/// TLS accept, then WebSocket accept, yielding a byte stream. The caller then
/// reads the rendezvous preamble and joins the `cretlink_relay::Rendezvous`
/// over the returned stream exactly as over raw TCP.
///
/// # Errors
///
/// Returns [`WssError::Io`] if the TLS handshake fails (including a plain-TCP
/// client that never negotiates TLS) or [`WssError::Handshake`] if the
/// WebSocket upgrade fails.
pub async fn accept<S>(
    acceptor: &TlsAcceptor,
    tcp: S,
) -> Result<WsByteStream<tokio_rustls::server::TlsStream<S>>, WssError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let tls = acceptor.accept(tcp).await.map_err(WssError::Io)?;
    let ws = tokio_tungstenite::accept_async(tls)
        .await
        .map_err(|e| WssError::Handshake(Box::new(e)))?;
    Ok(WsByteStream::new(ws))
}

/// Completes the server side of a WebSocket **without TLS** over an accepted
/// transport `tcp`, yielding a byte stream (ST-M007-015).
///
/// This is for a relay behind a reverse proxy that ends TLS: the endpoints
/// still speak `wss` to the proxy, and the proxy passes the WebSocket on to the
/// relay over a connection that the relay does not encrypt. That hop must be
/// one the operator trusts (the same host or a private network): the
/// rendezvous preamble with the admission key crosses it in the clear. The
/// session inside stays end-to-end encrypted either way.
///
/// # Errors
///
/// Returns [`WssError::Handshake`] if the WebSocket upgrade fails.
pub async fn accept_plain<S>(tcp: S) -> Result<WsByteStream<S>, WssError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ws = tokio_tungstenite::accept_async(tcp)
        .await
        .map_err(|e| WssError::Handshake(Box::new(e)))?;
    Ok(WsByteStream::new(ws))
}

/// The host of a request as the authority of its URI: an IPv6 address stands
/// in square brackets (RFC 3986, section 3.2.2), and anything else as it is.
/// Without the brackets the colons of the address read as the start of a port
/// and no request can be built, so a relay named by an IPv6 address could
/// never be reached.
fn authority(host: &str) -> String {
    match host.parse::<Ipv6Addr>() {
        Ok(_) => format!("[{host}]"),
        Err(_) => host.to_owned(),
    }
}

/// Completes the client side of a WebSocket **without TLS** over a connected
/// transport `tcp`: what a reverse proxy does towards a relay behind it
/// ([`accept_plain`]). The endpoints of a session never use this; they speak
/// `wss` ([`connect`]).
///
/// # Errors
///
/// Returns [`WssError::Handshake`] if the WebSocket upgrade fails.
pub async fn connect_plain<S>(host: &str, tcp: S) -> Result<WsByteStream<S>, WssError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = format!("ws://{}/", authority(host));
    let (ws, _response) = tokio_tungstenite::client_async(request, tcp)
        .await
        .map_err(|e| WssError::Handshake(Box::new(e)))?;
    Ok(WsByteStream::new(ws))
}

/// Completes the client side of the envelope over a connected transport `tcp`:
/// TLS connect (validating `server_name` against the config's trust anchors),
/// then WebSocket connect, yielding a byte stream to dial the relay over.
///
/// An untrusted or mismatched server certificate fails the TLS handshake here
/// and no stream is returned (fail closed).
///
/// # Errors
///
/// Returns [`WssError::InvalidServerName`] if `server_name` is neither a valid
/// DNS name nor an IP address, [`WssError::Io`] if the TLS handshake fails
/// (including an untrusted certificate), or [`WssError::Handshake`] if the
/// WebSocket upgrade fails.
pub async fn connect<S>(
    connector: &TlsConnector,
    server_name: &str,
    tcp: S,
) -> Result<WsByteStream<tokio_rustls::client::TlsStream<S>>, WssError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|_| WssError::InvalidServerName(server_name.to_owned()))?;
    let tls = connector.connect(name, tcp).await.map_err(WssError::Io)?;
    let request = format!("wss://{}/", authority(server_name));
    let (ws, _response) = tokio_tungstenite::client_async(request, tls)
        .await
        .map_err(|e| WssError::Handshake(Box::new(e)))?;
    Ok(WsByteStream::new(ws))
}
