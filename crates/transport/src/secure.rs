//! The post-handshake encrypted transport (ST-M002-004, ADR-0003).
//!
//! [`SecureChannel`] wraps a `snow` [`TransportState`] (the Noise
//! `XX_25519_ChaChaPoly_SHA256` record layer) together with the underlying byte
//! stream and sends [`Message`]s with confidentiality and integrity. It invents
//! no cryptography: every record is encrypted and authenticated by `snow`,
//! which advances a fresh per-message nonce, giving replay resistance
//! (REQ-S-008) and tamper detection for free.
//!
//! # Wire format
//!
//! Each message is one Noise record framed with a 4-byte little-endian `u32`
//! length prefix of the **ciphertext**. The plaintext is the JSON produced by
//! [`Message::encode`]; `snow` encrypts it and appends a 16-byte
//! ChaCha20-Poly1305 tag. `recv` rejects a declared ciphertext length above
//! [`MAX_CIPHERTEXT_LEN`] from the prefix alone, before allocating.
//!
//! # Fail-closed, single-use (REQ-S-007, REQ-S-009)
//!
//! Any `snow` error, decode error, oversized frame, or EOF **permanently
//! poisons** the channel: an internal flag is set, the failing call returns a
//! fatal error, and every later call returns [`SecureChannelError::Poisoned`].
//! A session is never resumed after a fault; the caller must re-pair. There is
//! deliberately no reconnect, retry, or skip-and-continue path — that
//! discipline is the lesson of RUSTSEC-2024-0011 baked into the type.
//!
//! # Cancel safety
//!
//! [`SecureChannel::recv`] is **cancel-safe**: an interrupted `recv` (for
//! example a losing `tokio::select!` branch) keeps its partially-read ciphertext
//! inside the channel's read state, so the next `recv` resumes exactly where it
//! left off. Decryption happens only once a whole record is buffered, with no
//! intervening `.await`, so a cancelled read can never desynchronise the record
//! layer or advance a Noise nonce mid-frame. This lets one task own the channel
//! and multiplex full-duplex traffic (requests, streamed output, heartbeats,
//! revocation) by racing `recv` against other sources. [`SecureChannel::send`]
//! is **not** cancel-safe: a dropped `send` future can leave a partial record on
//! the stream, so it must always be awaited to completion.
//!
//! # Record size ceiling
//!
//! A single Noise record carries at most `NOISE_MAX_PLAINTEXT_LEN` bytes of
//! plaintext. Messages whose encoded form exceeds that (for example a maximal
//! [`limits::MAX_CHUNK_BYTES`] job-output chunk) cannot traverse the channel in
//! one record and fail closed on send with a recoverable
//! [`SecureChannelError::Oversized`]; record-layer chunking is not implemented.
//!
//! A relay (REQ-F-022) sits beneath this channel: it forwards the encrypted
//! records unmodified and never sees plaintext.

use core::fmt;

use cretlink_protocol::{ErrorCode, Message, ProtocolError, limits};
use snow::TransportState;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use crate::frame::{FrameError, write_frame};

/// ChaCha20-Poly1305 authentication-tag length `snow` appends to every record.
pub(crate) const NOISE_TAG_LEN: usize = 16;

/// Maximum length of a single Noise message (the Noise spec `MAXMSGLEN`).
pub(crate) const NOISE_MAX_MESSAGE_LEN: usize = 65535;

/// Maximum plaintext a single Noise record can carry (message max minus tag).
pub(crate) const NOISE_MAX_PLAINTEXT_LEN: usize = NOISE_MAX_MESSAGE_LEN - NOISE_TAG_LEN;

// What builds an answer to fit one record (a folder page, ST-M010-005) cannot
// see this crate, so it measures against the protocol's bound. The two are the
// same number, or such an answer would be built for a record that is not this
// one.
const _: () = assert!(NOISE_MAX_PLAINTEXT_LEN == limits::MAX_RECORD_BYTES);

/// Ciphertext-length ceiling enforced on a received frame before allocation: a
/// full [`limits::MAX_FRAME_BYTES`] plaintext plus the Noise tag.
pub const MAX_CIPHERTEXT_LEN: usize = limits::MAX_FRAME_BYTES + NOISE_TAG_LEN;

/// A fatal error on the encrypted transport. Every variant is terminal: once
/// any of them is produced the [`SecureChannel`] is poisoned (REQ-S-009).
#[derive(Debug, Error)]
pub enum SecureChannelError {
    /// An I/O failure on the underlying stream.
    #[error("secure channel I/O error")]
    Io(#[source] std::io::Error),

    /// A declared ciphertext frame length exceeded [`MAX_CIPHERTEXT_LEN`],
    /// detected before allocation. Maps to
    /// [`ErrorCode::OversizedMessage`].
    #[error("ciphertext frame of {len} bytes exceeds the {max}-byte limit")]
    Oversized {
        /// The declared length in bytes.
        len: usize,
        /// The enforced ceiling in bytes.
        max: usize,
    },

    /// A `snow` record could not be encrypted or decrypted: a tampered frame, a
    /// replayed frame (nonce mismatch), an over-large record, or exhausted
    /// nonces. The detailed cause is deliberately not surfaced to peers.
    #[error("noise record could not be encrypted or decrypted")]
    Noise(#[source] snow::Error),

    /// A decrypted frame was not a valid protocol [`Message`] (bad JSON,
    /// unknown field, or a violated size limit).
    #[error("decrypted frame was not a valid protocol message")]
    Decode(#[source] ProtocolError),

    /// The peer closed the stream, cleanly or mid-frame. Any in-flight work has
    /// an unknown outcome (REQ-N-008).
    #[error("secure channel closed by peer")]
    Eof,

    /// The channel was already torn down by an earlier fatal error and refuses
    /// all further work (REQ-S-009, fail-closed).
    #[error("secure channel is permanently closed after an earlier fatal error")]
    Poisoned,
}

impl SecureChannelError {
    fn from_frame(error: FrameError) -> Self {
        match error {
            FrameError::Io(io) => Self::Io(io),
            FrameError::Oversized { len, max } => Self::Oversized { len, max },
            FrameError::Eof => Self::Eof,
        }
    }

    /// The wire [`ErrorCode`] a peer should receive for this failure, mirroring
    /// [`ProtocolError::code`](cretlink_protocol::ProtocolError). Every fault on
    /// this channel is terminal, so the crypto, EOF, poison, and I/O variants
    /// all report [`ErrorCode::SessionRevoked`] — the peer should stop sending
    /// work and re-pair — while size and decode faults keep their specific
    /// categories.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Oversized { .. } => ErrorCode::OversizedMessage,
            Self::Decode(_) => ErrorCode::MalformedMessage,
            Self::Noise(_) | Self::Eof | Self::Poisoned | Self::Io(_) => ErrorCode::SessionRevoked,
        }
    }
}

/// Progress of the cancel-safe ciphertext receive state machine. A partially
/// read frame is retained here across `recv` cancellations so nothing is lost
/// and the record layer never desynchronises.
#[derive(Debug)]
enum ReadState {
    /// Reading the 4-byte little-endian ciphertext-length prefix.
    Prefix {
        /// Prefix bytes received so far.
        buf: [u8; 4],
        /// How many prefix bytes are valid.
        filled: usize,
    },
    /// Reading the ciphertext body, whose declared length already passed the
    /// size ceiling.
    Payload {
        /// Ciphertext buffer, sized to the declared length.
        buf: Vec<u8>,
        /// How many ciphertext bytes are valid.
        filled: usize,
    },
}

impl ReadState {
    const fn start() -> Self {
        Self::Prefix {
            buf: [0; 4],
            filled: 0,
        }
    }
}

/// An authenticated, confidential message channel over a byte stream.
///
/// Obtained from [`crate::Pairing::perform`]; see the module docs for the wire
/// format and the fail-closed, single-use contract.
pub struct SecureChannel<T> {
    transport: TransportState,
    io: T,
    poisoned: bool,
    read_state: ReadState,
}

// `snow::TransportState` does not implement `Debug`, and its key material must
// never be printed regardless. Redact everything but the liveness flag.
impl<T> fmt::Debug for SecureChannel<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecureChannel")
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl<T> SecureChannel<T> {
    /// Wraps a completed Noise transport state and its stream.
    pub(crate) const fn new(transport: TransportState, io: T) -> Self {
        Self {
            transport,
            io,
            poisoned: false,
            read_state: ReadState::start(),
        }
    }

    /// Whether the channel has been poisoned by an earlier fatal error and will
    /// reject all further work.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Consumes the channel and returns the underlying stream, dropping the
    /// Noise transport state.
    ///
    /// Dropping the [`TransportState`] releases `snow`'s session keys. Note that
    /// `snow` 0.10's default resolver does not zeroize its cipher keys on drop,
    /// so residual key bytes may remain in freed memory; the session is
    /// nonetheless invalidated here (single-use) and cannot be resumed, and
    /// CretLink's own secrets — the pairing nonces and the fresh static key —
    /// are erased explicitly during pairing.
    pub fn into_inner(self) -> T {
        self.io
    }

    /// Tears the channel down, invalidating the session and returning the
    /// stream so the caller can close it. Alias for [`Self::into_inner`] that
    /// reads as an explicit closure at call sites (REQ-S-007).
    pub fn close(self) -> T {
        self.into_inner()
    }
}

impl<T: AsyncWrite + Unpin> SecureChannel<T> {
    /// Encrypts and sends one [`Message`].
    ///
    /// The message is validated and encoded first; a message that violates a
    /// protocol limit is a caller error that never touches the transport and
    /// therefore does **not** poison the channel. Any failure once encryption
    /// or the stream is involved is fatal and poisons the channel.
    pub async fn send(&mut self, message: &Message) -> Result<(), SecureChannelError> {
        if self.poisoned {
            return Err(SecureChannelError::Poisoned);
        }
        // Validation/encoding happens before any transport state changes, so a
        // rejected message leaves the channel usable.
        let payload = message.encode().map_err(SecureChannelError::Decode)?;
        // A single Noise record cannot carry more than NOISE_MAX_PLAINTEXT_LEN
        // bytes of plaintext. Reject that here — before touching the transport
        // state — so an over-large message is a recoverable caller error rather
        // than a poisoning fault. (Record-layer chunking for large payloads is
        // not implemented.)
        if payload.len() > NOISE_MAX_PLAINTEXT_LEN {
            return Err(SecureChannelError::Oversized {
                len: payload.len(),
                max: NOISE_MAX_PLAINTEXT_LEN,
            });
        }
        match self.send_encrypted(&payload).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    async fn send_encrypted(&mut self, payload: &[u8]) -> Result<(), SecureChannelError> {
        let mut ciphertext = vec![0u8; payload.len() + NOISE_TAG_LEN];
        let n = self
            .transport
            .write_message(payload, &mut ciphertext)
            .map_err(SecureChannelError::Noise)?;
        write_frame(&mut self.io, &ciphertext[..n], MAX_CIPHERTEXT_LEN)
            .await
            .map_err(SecureChannelError::from_frame)
    }
}

impl<T: AsyncRead + Unpin> SecureChannel<T> {
    /// Receives, decrypts, and decodes one [`Message`].
    ///
    /// Any framing, decryption, or decode failure — including a tampered or
    /// replayed frame — is fatal and permanently poisons the channel.
    pub async fn recv(&mut self) -> Result<Message, SecureChannelError> {
        if self.poisoned {
            return Err(SecureChannelError::Poisoned);
        }
        match self.recv_decrypted().await {
            Ok(message) => Ok(message),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    async fn recv_decrypted(&mut self) -> Result<Message, SecureChannelError> {
        let ciphertext = self.read_ciphertext_frame().await?;
        // The whole record is now buffered. Everything below is synchronous —
        // there is no `.await` before returning — so a cancelled `recv` can only
        // ever be interrupted inside `read_ciphertext_frame` (which retains its
        // partial read), never between reading a full record and decrypting it.
        //
        // The plaintext is always shorter than the ciphertext (which includes
        // the tag), so the ciphertext length is a safe output-buffer size.
        let mut plaintext = vec![0u8; ciphertext.len()];
        let n = self
            .transport
            .read_message(&ciphertext, &mut plaintext)
            .map_err(SecureChannelError::Noise)?;
        Message::decode(&plaintext[..n]).map_err(SecureChannelError::Decode)
    }

    /// Reads one length-prefixed ciphertext frame, cancel-safely: partial reads
    /// live in `self.read_state`, so a dropped future resumes cleanly. The
    /// declared length is checked against [`MAX_CIPHERTEXT_LEN`] from the prefix
    /// alone, before allocating the body buffer.
    async fn read_ciphertext_frame(&mut self) -> Result<Vec<u8>, SecureChannelError> {
        loop {
            match &mut self.read_state {
                ReadState::Prefix { buf, filled } => {
                    if *filled == buf.len() {
                        let len = u32::from_le_bytes(*buf) as usize;
                        if len > MAX_CIPHERTEXT_LEN {
                            return Err(SecureChannelError::Oversized {
                                len,
                                max: MAX_CIPHERTEXT_LEN,
                            });
                        }
                        self.read_state = ReadState::Payload {
                            buf: vec![0u8; len],
                            filled: 0,
                        };
                    } else {
                        let read = self
                            .io
                            .read(&mut buf[*filled..])
                            .await
                            .map_err(SecureChannelError::Io)?;
                        if read == 0 {
                            return Err(SecureChannelError::Eof);
                        }
                        *filled += read;
                    }
                }
                ReadState::Payload { buf, filled } => {
                    if *filled == buf.len() {
                        let payload = core::mem::take(buf);
                        self.read_state = ReadState::start();
                        return Ok(payload);
                    }
                    let read = self
                        .io
                        .read(&mut buf[*filled..])
                        .await
                        .map_err(SecureChannelError::Io)?;
                    if read == 0 {
                        return Err(SecureChannelError::Eof);
                    }
                    *filled += read;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_map_to_sensible_wire_categories() {
        assert_eq!(
            SecureChannelError::Oversized { len: 1, max: 0 }.code(),
            ErrorCode::OversizedMessage
        );
        assert_eq!(
            SecureChannelError::Poisoned.code(),
            ErrorCode::SessionRevoked
        );
        assert_eq!(SecureChannelError::Eof.code(), ErrorCode::SessionRevoked);
    }

    #[test]
    fn ciphertext_ceiling_leaves_room_for_a_full_frame_plus_tag() {
        assert_eq!(MAX_CIPHERTEXT_LEN, limits::MAX_FRAME_BYTES + NOISE_TAG_LEN);
        assert_eq!(
            NOISE_MAX_PLAINTEXT_LEN,
            NOISE_MAX_MESSAGE_LEN - NOISE_TAG_LEN
        );
    }
}
