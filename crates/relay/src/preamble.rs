//! Rendezvous preamble codec — the tiny framing each endpoint writes *once*
//! before the relay begins forwarding (ST-M007-007, ADR-0016 §5).
//!
//! # Why a preamble at all
//!
//! The relay core is transport-agnostic: [`splice`](crate::splice::splice)
//! forwards opaque bytes and [`Rendezvous`](crate::rendezvous::Rendezvous) pairs
//! two legs by a [`RendezvousTicket`]. Something has to carry, on the wire, the
//! two pieces the relay needs *before* it can admit and pair a fresh connection:
//!
//! 1. an optional admission credential ([`AdmissionToken`]) — abuse control only,
//!    **never** session authority ([`admission`](crate::admission)); and
//! 2. the single-use [`RendezvousTicket`] naming which pending pairing this leg
//!    belongs to.
//!
//! This module is that framing, and it lives in the relay leaf core so both the
//! relay-server binary (which *reads* it, ST-M007-007) and the endpoint dialer
//! (which *writes* it, ST-M007-008) share one definition with no dependency
//! cycle.
//!
//! # Wire layout
//!
//! ```text
//! magic      4 bytes   b"CRLR"  (CretLink Relay Rendezvous)
//! version    1 byte    PREAMBLE_VERSION
//! token_len  2 bytes   little-endian, 0..=MAX_PREAMBLE_TOKEN_BYTES
//! token      token_len bytes (absent when token_len == 0)
//! ticket     RENDEZVOUS_TICKET_LEN bytes
//! ```
//!
//! The preamble is **plaintext framing, not session data**: it addresses the
//! forwarder and is consumed by it. Everything after it is the unmodified
//! end-to-end Noise session the relay only ever sees as ciphertext (ADR-0010),
//! so this codec never touches a session key and the token is the *only* secret
//! it handles — held in the zeroizing [`AdmissionToken`] and never logged.
//!
//! # The relay's answer
//!
//! The relay answers every preamble once, before it forwards anything
//! (ST-M007-012, ADR-0016 §5):
//!
//! ```text
//! magic      4 bytes   b"CRLR"
//! version    1 byte    the relay's PREAMBLE_VERSION
//! status     1 byte    0 = admitted; 1..=6 = refused, see RelayRefusal
//! ```
//!
//! *Admitted* means that the leg waits for its peer, or has met it: whatever
//! follows on the stream comes from the peer and is opaque to the relay.
//! Without the answer a refusal, a full relay and a rendezvous that nobody
//! joined all looked the same to an endpoint, a closed connection. With it an
//! endpoint tells them apart: a refusal is said, and a stream that ends after
//! *admitted* before the peer was heard is a rendezvous that lapsed.
//!
//! The answer is the relay's own addressing metadata. It is fixed in size,
//! written once, carries nothing of a session and gives nobody control: the
//! reasons are those of admission (ADR-0016 §4), which decides only who may
//! use the forwarder.
//!
//! # Fail-closed reading
//!
//! [`read_preamble`] validates every field with a hard bound *before* it
//! allocates or trusts anything (REQ-N-006, REQ-S-009): a wrong magic, an
//! unsupported version, or a `token_len` past [`MAX_PREAMBLE_TOKEN_BYTES`] is
//! rejected as a typed [`PreambleError`] with no oversized allocation and no
//! forwarding. A short read (truncated preamble) surfaces as
//! [`PreambleError::Io`]. The caller is expected to run this under its own
//! timeout and to forward **zero** bytes on any error.
//!

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::admission::AdmissionToken;
use crate::ticket::{RENDEZVOUS_TICKET_LEN, RendezvousTicket};

/// The four-byte magic that opens every rendezvous preamble: `CRLR`, for
/// *CretLink Relay Rendezvous*. It only guards against a wrong protocol dialing
/// the port; it is not a secret and provides no authentication.
pub const PREAMBLE_MAGIC: [u8; 4] = *b"CRLR";

/// The preamble wire version this build reads and writes. A relay refuses
/// another version and says so in its answer ([`RelayRefusal::UnsupportedVersion`]).
///
/// Version 2 added the relay's answer ([`RelayAnswer`]); a version 1 relay
/// answers nothing.
pub const PREAMBLE_VERSION: u8 = 2;

/// Length in bytes of the relay's answer to a preamble.
pub const RELAY_ANSWER_LEN: usize = 6;

/// The status byte of an admitted leg.
const STATUS_ADMITTED: u8 = 0;

/// The relay's answer to a rendezvous preamble. See the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayAnswer {
    /// The leg may use the forwarder. It waits for its peer or has met it;
    /// everything that follows on the stream comes from the peer.
    Admitted,
    /// The leg is refused. The relay closes the connection after the answer
    /// and forwards nothing.
    Refused(RelayRefusal),
}

/// Why a relay refused a leg. Every reason is secret-free and says what to do
/// next (REQ-N-015). None of them concerns a session: a relay decides only who
/// may use the forwarder (ADR-0016 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RelayRefusal {
    /// The relay admits only with an admission key and none was presented.
    #[error("the relay asks for an admission key and none was presented")]
    CredentialRequired,
    /// The relay does not know the admission key that was presented.
    #[error("the relay does not know the admission key that was presented")]
    CredentialRejected,
    /// Too many attempts in a short time. Wait before trying again.
    #[error("the relay had too many attempts in a short time; wait a moment")]
    RateLimited,
    /// The relay, or this client's share of it, has no free place.
    #[error("the relay has no free place right now")]
    Busy,
    /// The relay cannot admit anybody right now and needs its operator.
    #[error("the relay cannot admit connections right now")]
    Unavailable,
    /// The relay speaks another version of the rendezvous preamble: the relay
    /// server and this program come from releases that do not fit together.
    #[error("the relay speaks version {relay} of the rendezvous preamble, this program another")]
    UnsupportedVersion {
        /// The preamble version the relay speaks.
        relay: u8,
    },
}

impl RelayRefusal {
    /// The status byte of this refusal on the wire.
    const fn status(self) -> u8 {
        match self {
            Self::CredentialRequired => 1,
            Self::CredentialRejected => 2,
            Self::RateLimited => 3,
            Self::Busy => 4,
            Self::Unavailable => 5,
            Self::UnsupportedVersion { .. } => STATUS_UNSUPPORTED_VERSION,
        }
    }
}

/// The status byte of [`RelayRefusal::UnsupportedVersion`]. It keeps its
/// meaning in every preamble version, so that two versions can at least say
/// that they differ.
const STATUS_UNSUPPORTED_VERSION: u8 = 6;

/// The largest admission token, in bytes, the preamble will carry. A bearer
/// credential is small; this cap bounds the pre-splice allocation so a malicious
/// `token_len` cannot force a large buffer before the connection is even admitted
/// (REQ-N-006). It sits well under `u16::MAX`, so the two length bytes never
/// overflow.
pub const MAX_PREAMBLE_TOKEN_BYTES: usize = 512;

/// A decoded rendezvous preamble: the optional admission credential and the
/// single-use ticket naming the pairing.
///
/// [`Debug`] stays redacted through both fields — [`AdmissionToken`] renders as
/// `<redacted>` and [`RendezvousTicket`] hides its bytes — so a decoded preamble
/// is safe to include in an error or metadata line.
#[derive(Debug, Clone)]
pub struct Preamble {
    /// The admission credential the dialer presented, or `None` when it sent
    /// none (an [`AdmissionPolicy::Open`](crate::admission::AdmissionPolicy::Open)
    /// relay). Admission decides whether `None` is acceptable — the codec does
    /// not.
    pub token: Option<AdmissionToken>,
    /// The single-use ticket that names which pending pairing this leg joins.
    pub ticket: RendezvousTicket,
}

impl Preamble {
    /// Builds a preamble from its parts.
    #[must_use]
    pub fn new(token: Option<AdmissionToken>, ticket: RendezvousTicket) -> Self {
        Self { token, ticket }
    }
}

/// Why encoding or decoding a rendezvous preamble failed. Every variant is
/// secret-free and actionable (REQ-N-015); none ever embeds the token bytes.
#[derive(Debug, Error)]
pub enum PreambleError {
    /// The leading bytes were not [`PREAMBLE_MAGIC`]: the peer is not speaking the
    /// CretLink rendezvous framing (a port scan, a wrong protocol, or a proxy).
    #[error("rendezvous preamble magic mismatch: not a CretLink relay connection")]
    BadMagic,
    /// The version byte named a wire version this relay does not implement.
    #[error("unsupported rendezvous preamble version {found} (this relay speaks {expected})")]
    UnsupportedVersion {
        /// The version the peer sent.
        found: u8,
        /// The version this build speaks ([`PREAMBLE_VERSION`]).
        expected: u8,
    },
    /// The declared token length exceeded [`MAX_PREAMBLE_TOKEN_BYTES`]. Detected
    /// from the length field alone, *before* any buffer is allocated.
    #[error("admission token length {len} exceeds the {max}-byte preamble limit")]
    TokenTooLong {
        /// The declared length.
        len: usize,
        /// The accepted maximum ([`MAX_PREAMBLE_TOKEN_BYTES`]).
        max: usize,
    },
    /// The relay's answer carried a status this build does not know. It is
    /// never taken for an admission.
    #[error("the relay answered with an unknown status {status}")]
    UnknownAnswer {
        /// The status byte the relay sent.
        status: u8,
    },
    /// The underlying stream failed or ended mid-preamble (a truncated or reset
    /// connection). Reading fails closed; the caller forwards nothing.
    #[error("reading the rendezvous preamble failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Reads and validates a rendezvous preamble from `reader`, accepting an
/// admission token no larger than `max_token_bytes`.
///
/// `max_token_bytes` is clamped to [`MAX_PREAMBLE_TOKEN_BYTES`]; a caller may
/// pass a smaller bound but never a larger one. The length field is checked
/// against that bound *before* the token buffer is allocated, so an oversized
/// declaration is rejected without cost.
///
/// This performs the minimum reads needed and returns as soon as the ticket is
/// in hand; it does **not** consume any subsequent session bytes. Run it under a
/// timeout: a peer that stalls mid-preamble otherwise holds the read open.
///
/// # Errors
///
/// [`PreambleError`] — a wrong magic, unsupported version, oversized token
/// length, or a truncated/failed stream. On any error the caller must forward
/// zero bytes (REQ-S-009).
pub async fn read_preamble<R>(
    reader: &mut R,
    max_token_bytes: usize,
) -> Result<Preamble, PreambleError>
where
    R: AsyncRead + Unpin,
{
    let cap = max_token_bytes.min(MAX_PREAMBLE_TOKEN_BYTES);

    let mut magic = [0u8; PREAMBLE_MAGIC.len()];
    reader.read_exact(&mut magic).await?;
    if magic != PREAMBLE_MAGIC {
        return Err(PreambleError::BadMagic);
    }

    let version = reader.read_u8().await?;
    if version != PREAMBLE_VERSION {
        return Err(PreambleError::UnsupportedVersion {
            found: version,
            expected: PREAMBLE_VERSION,
        });
    }

    // Bound the declared length *before* allocating the token buffer.
    let token_len = usize::from(reader.read_u16_le().await?);
    if token_len > cap {
        return Err(PreambleError::TokenTooLong {
            len: token_len,
            max: cap,
        });
    }

    let token = if token_len == 0 {
        None
    } else {
        let mut buf = vec![0u8; token_len];
        reader.read_exact(&mut buf).await?;
        Some(AdmissionToken::new(buf))
    };

    let mut ticket_bytes = [0u8; RENDEZVOUS_TICKET_LEN];
    reader.read_exact(&mut ticket_bytes).await?;
    let ticket = RendezvousTicket::from_bytes(ticket_bytes);

    Ok(Preamble { token, ticket })
}

/// Encodes `preamble` onto `writer` and flushes it.
///
/// The whole preamble is assembled into one buffer and written with a single
/// [`write_all`](AsyncWriteExt::write_all), so it reaches the relay as an
/// uninterrupted unit. This is the write path the endpoint dialer uses
/// (ST-M007-008); the relay-server only ever reads.
///
/// # Errors
///
/// [`PreambleError::TokenTooLong`] if the token exceeds
/// [`MAX_PREAMBLE_TOKEN_BYTES`] (a symmetric refusal to emit what no relay would
/// accept), or [`PreambleError::Io`] if the write fails.
pub async fn write_preamble<W>(writer: &mut W, preamble: &Preamble) -> Result<(), PreambleError>
where
    W: AsyncWrite + Unpin,
{
    let token_bytes = preamble.token.as_ref().map(AdmissionToken::as_bytes);
    let token_len = token_bytes.map_or(0, <[u8]>::len);
    if token_len > MAX_PREAMBLE_TOKEN_BYTES {
        return Err(PreambleError::TokenTooLong {
            len: token_len,
            max: MAX_PREAMBLE_TOKEN_BYTES,
        });
    }
    // token_len <= MAX_PREAMBLE_TOKEN_BYTES < u16::MAX, so the cast cannot wrap.
    let len_field = (token_len as u16).to_le_bytes();

    let mut buf =
        Vec::with_capacity(PREAMBLE_MAGIC.len() + 1 + 2 + token_len + RENDEZVOUS_TICKET_LEN);
    buf.extend_from_slice(&PREAMBLE_MAGIC);
    buf.push(PREAMBLE_VERSION);
    buf.extend_from_slice(&len_field);
    if let Some(bytes) = token_bytes {
        buf.extend_from_slice(bytes);
    }
    buf.extend_from_slice(preamble.ticket.as_bytes());

    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

/// Writes the relay's answer to a preamble and flushes it. The relay calls
/// this once per connection, before it parks or splices the leg.
///
/// # Errors
///
/// Returns the I/O error of a failed write; the relay then drops the leg.
pub async fn write_answer<W>(writer: &mut W, answer: RelayAnswer) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let status = match answer {
        RelayAnswer::Admitted => STATUS_ADMITTED,
        RelayAnswer::Refused(refusal) => refusal.status(),
    };
    let mut frame = [0u8; RELAY_ANSWER_LEN];
    frame[..PREAMBLE_MAGIC.len()].copy_from_slice(&PREAMBLE_MAGIC);
    frame[4] = PREAMBLE_VERSION;
    frame[5] = status;
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Reads the relay's answer to the preamble this endpoint wrote. It consumes
/// exactly [`RELAY_ANSWER_LEN`] bytes and nothing of what follows. Run it under
/// a timeout: a relay that stalls otherwise holds the read open.
///
/// A relay of another preamble version is reported as
/// [`RelayRefusal::UnsupportedVersion`], whatever status it sent, because its
/// other statuses may mean something else.
///
/// # Errors
///
/// [`PreambleError::BadMagic`] if the peer is no CretLink relay,
/// [`PreambleError::UnknownAnswer`] for a status this build does not know, and
/// [`PreambleError::Io`] if the stream failed or ended first, as it does with
/// a version 1 relay, which answers nothing. An error is never an admission.
pub async fn read_answer<R>(reader: &mut R) -> Result<RelayAnswer, PreambleError>
where
    R: AsyncRead + Unpin,
{
    let mut frame = [0u8; RELAY_ANSWER_LEN];
    reader.read_exact(&mut frame).await?;
    if frame[..PREAMBLE_MAGIC.len()] != PREAMBLE_MAGIC {
        return Err(PreambleError::BadMagic);
    }
    let (version, status) = (frame[4], frame[5]);
    if status == STATUS_UNSUPPORTED_VERSION || version != PREAMBLE_VERSION {
        return Ok(RelayAnswer::Refused(RelayRefusal::UnsupportedVersion {
            relay: version,
        }));
    }
    let refusal = match status {
        STATUS_ADMITTED => return Ok(RelayAnswer::Admitted),
        1 => RelayRefusal::CredentialRequired,
        2 => RelayRefusal::CredentialRejected,
        3 => RelayRefusal::RateLimited,
        4 => RelayRefusal::Busy,
        5 => RelayRefusal::Unavailable,
        status => return Err(PreambleError::UnknownAnswer { status }),
    };
    Ok(RelayAnswer::Refused(refusal))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const REFUSALS: [RelayRefusal; 6] = [
        RelayRefusal::CredentialRequired,
        RelayRefusal::CredentialRejected,
        RelayRefusal::RateLimited,
        RelayRefusal::Busy,
        RelayRefusal::Unavailable,
        RelayRefusal::UnsupportedVersion {
            relay: PREAMBLE_VERSION,
        },
    ];

    async fn answer_round_trip(answer: RelayAnswer) -> RelayAnswer {
        let mut buf = Vec::new();
        write_answer(&mut buf, answer).await.unwrap();
        assert_eq!(buf.len(), RELAY_ANSWER_LEN);
        read_answer(&mut buf.as_slice()).await.unwrap()
    }

    #[tokio::test]
    async fn every_answer_round_trips() {
        assert_eq!(
            answer_round_trip(RelayAnswer::Admitted).await,
            RelayAnswer::Admitted
        );
        for refusal in REFUSALS {
            assert_eq!(
                answer_round_trip(RelayAnswer::Refused(refusal)).await,
                RelayAnswer::Refused(refusal)
            );
        }
    }

    #[tokio::test]
    async fn the_answer_is_frozen_on_the_wire() {
        // FROZEN: magic, the relay's preamble version, the status.
        let mut buf = Vec::new();
        write_answer(&mut buf, RelayAnswer::Admitted).await.unwrap();
        assert_eq!(buf, [b'C', b'R', b'L', b'R', 2, 0]);
        buf.clear();
        write_answer(
            &mut buf,
            RelayAnswer::Refused(RelayRefusal::CredentialRejected),
        )
        .await
        .unwrap();
        assert_eq!(buf, [b'C', b'R', b'L', b'R', 2, 2]);
        // Every refusal has a status of its own, and none is the admission.
        let mut statuses: Vec<u8> = REFUSALS.iter().map(|refusal| refusal.status()).collect();
        statuses.sort_unstable();
        assert_eq!(statuses, [1, 2, 3, 4, 5, 6]);
    }

    #[tokio::test]
    async fn reading_an_answer_leaves_the_peer_stream_untouched() {
        let mut bytes = Vec::new();
        write_answer(&mut bytes, RelayAnswer::Admitted)
            .await
            .unwrap();
        bytes.extend_from_slice(b"opaque session bytes");
        let mut reader = bytes.as_slice();
        assert_eq!(
            read_answer(&mut reader).await.unwrap(),
            RelayAnswer::Admitted
        );
        assert_eq!(reader, b"opaque session bytes");
    }

    #[tokio::test]
    async fn an_unknown_status_is_never_an_admission() {
        let bytes = [b'C', b'R', b'L', b'R', PREAMBLE_VERSION, 0x7f];
        let err = read_answer(&mut bytes.as_slice()).await.unwrap_err();
        assert!(
            matches!(err, PreambleError::UnknownAnswer { status: 0x7f }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_relay_of_another_version_is_reported_as_such() {
        // Its version byte differs: whatever status it sent, even the one
        // that means "admitted" here, is read as a version mismatch.
        for status in [0u8, 2, 6, 0x7f] {
            let bytes = [b'C', b'R', b'L', b'R', PREAMBLE_VERSION + 1, status];
            assert_eq!(
                read_answer(&mut bytes.as_slice()).await.unwrap(),
                RelayAnswer::Refused(RelayRefusal::UnsupportedVersion {
                    relay: PREAMBLE_VERSION + 1
                }),
                "status {status}"
            );
        }
    }

    #[tokio::test]
    async fn an_answer_from_something_else_or_none_at_all_fails_closed() {
        let not_a_relay = *b"HTTP/1";
        let err = read_answer(&mut not_a_relay.as_slice()).await.unwrap_err();
        assert!(matches!(err, PreambleError::BadMagic), "{err:?}");
        // A version 1 relay answers nothing and closes.
        let nothing: [u8; 0] = [];
        let err = read_answer(&mut nothing.as_slice()).await.unwrap_err();
        assert!(matches!(err, PreambleError::Io(_)), "{err:?}");
        // A truncated answer.
        let err = read_answer(&mut PREAMBLE_MAGIC.as_slice())
            .await
            .unwrap_err();
        assert!(matches!(err, PreambleError::Io(_)), "{err:?}");
    }

    async fn round_trip(preamble: &Preamble) -> Preamble {
        let mut buf = Vec::new();
        write_preamble(&mut buf, preamble).await.unwrap();
        read_preamble(&mut buf.as_slice(), MAX_PREAMBLE_TOKEN_BYTES)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn round_trips_with_a_token() {
        let ticket = RendezvousTicket::from_bytes([7u8; RENDEZVOUS_TICKET_LEN]);
        let preamble = Preamble::new(Some(AdmissionToken::from_text("s3cret")), ticket.clone());

        let decoded = round_trip(&preamble).await;

        assert_eq!(decoded.ticket.as_bytes(), ticket.as_bytes());
        assert_eq!(
            decoded.token.as_ref().map(AdmissionToken::as_bytes),
            Some(b"s3cret".as_slice())
        );
    }

    #[tokio::test]
    async fn round_trips_without_a_token() {
        let ticket = RendezvousTicket::from_bytes([3u8; RENDEZVOUS_TICKET_LEN]);
        let preamble = Preamble::new(None, ticket.clone());

        let decoded = round_trip(&preamble).await;

        assert_eq!(decoded.ticket.as_bytes(), ticket.as_bytes());
        assert!(decoded.token.is_none());
    }

    #[tokio::test]
    async fn rejects_a_wrong_magic() {
        let mut bytes = vec![b'X', b'X', b'X', b'X', PREAMBLE_VERSION, 0, 0];
        bytes.extend_from_slice(&[0u8; RENDEZVOUS_TICKET_LEN]);
        let err = read_preamble(&mut bytes.as_slice(), MAX_PREAMBLE_TOKEN_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(err, PreambleError::BadMagic), "{err:?}");
    }

    #[tokio::test]
    async fn rejects_an_unsupported_version() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&PREAMBLE_MAGIC);
        bytes.push(PREAMBLE_VERSION.wrapping_add(1));
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&[0u8; RENDEZVOUS_TICKET_LEN]);
        let err = read_preamble(&mut bytes.as_slice(), MAX_PREAMBLE_TOKEN_BYTES)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PreambleError::UnsupportedVersion {
                    expected: PREAMBLE_VERSION,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_an_oversized_token_length_before_allocating() {
        // Header only: magic + version + a length far past the cap, and *no*
        // token bytes following. If the reader tried to allocate/read the
        // declared token it would block on the short stream; instead it must
        // reject from the length field alone.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&PREAMBLE_MAGIC);
        bytes.push(PREAMBLE_VERSION);
        bytes.extend_from_slice(&50_000u16.to_le_bytes());
        let err = read_preamble(&mut bytes.as_slice(), MAX_PREAMBLE_TOKEN_BYTES)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PreambleError::TokenTooLong {
                    len: 50_000,
                    max: MAX_PREAMBLE_TOKEN_BYTES
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn honours_a_caller_bound_tighter_than_the_cap() {
        // A 100-byte token is under the hard cap but over a caller's 16-byte
        // bound; the tighter bound must win.
        let ticket = RendezvousTicket::from_bytes([1u8; RENDEZVOUS_TICKET_LEN]);
        let preamble = Preamble::new(Some(AdmissionToken::new(vec![0xABu8; 100])), ticket.clone());
        let mut buf = Vec::new();
        write_preamble(&mut buf, &preamble).await.unwrap();

        let err = read_preamble(&mut buf.as_slice(), 16).await.unwrap_err();
        assert!(
            matches!(err, PreambleError::TokenTooLong { len: 100, max: 16 }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_truncated_preamble_is_an_io_error() {
        // Only the magic arrives, then EOF.
        let err = read_preamble(&mut PREAMBLE_MAGIC.as_slice(), MAX_PREAMBLE_TOKEN_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(err, PreambleError::Io(_)), "{err:?}");
    }

    #[tokio::test]
    async fn writing_refuses_an_oversized_token() {
        let ticket = RendezvousTicket::from_bytes([0u8; RENDEZVOUS_TICKET_LEN]);
        let preamble = Preamble::new(
            Some(AdmissionToken::new(vec![0u8; MAX_PREAMBLE_TOKEN_BYTES + 1])),
            ticket,
        );
        let mut buf = Vec::new();
        let err = write_preamble(&mut buf, &preamble).await.unwrap_err();
        assert!(matches!(err, PreambleError::TokenTooLong { .. }), "{err:?}");
        assert!(buf.is_empty(), "nothing should be written on refusal");
    }
}
