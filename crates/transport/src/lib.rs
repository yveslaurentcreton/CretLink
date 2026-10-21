//! Length-prefixed message framing for CretLink (ST-M001-003, ADR-0010).
//!
//! # Wire format
//!
//! Every frame is a 4-byte **little-endian `u32` length prefix** followed by
//! exactly that many bytes of JSON payload produced by
//! [`Message::encode`](cretlink_protocol::Message::encode). The receive path
//! checks the declared length against
//! [`limits::MAX_FRAME_BYTES`]
//! **before** allocating a receive buffer, so a hostile or corrupt prefix
//! fails closed without memory cost (REQ-N-012 bounded inputs).
//!
//! # Plaintext framing
//!
//! This layer carries plaintext frames only. [`SecureChannel`] (ADR-0003)
//! wraps this framing for real transports; bare [`Framed`] serves in-process
//! and test transports such as `tokio::io::duplex`.
//!
//! # Cancel safety
//!
//! [`Framed::recv`] is cancel-safe: partial reads are kept inside the
//! [`Framed`] state machine, so a `recv` future dropped mid-frame (for
//! example by `tokio::select!`) loses nothing and the next call resumes
//! where it left off. [`Framed::send`] is **not** cancel-safe: dropping a
//! `send` future mid-write can leave a partial frame on the stream.
//!
//! # Error handling
//!
//! After [`TransportError::Oversized`] the stream is desynchronised (the
//! offending payload was never consumed) and the connection must be
//! dropped. After [`TransportError::Decode`] the framing itself is still
//! intact and further frames can be received, but peers are expected to
//! fail closed. Remote frames are untrusted input.

use cretlink_protocol::{Message, ProtocolError, limits};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

mod frame;
pub mod pairing;
pub mod secure;

pub use pairing::{Paired, Pairing, PairingError, Role};
pub use secure::{MAX_CIPHERTEXT_LEN, SecureChannel, SecureChannelError};

/// A transport-level send or receive failure.
#[derive(Debug, Error)]
pub enum TransportError {
    /// The underlying stream failed.
    #[error("transport i/o failure")]
    Io(#[from] std::io::Error),

    /// A frame length exceeded
    /// [`limits::MAX_FRAME_BYTES`].
    /// On the receive path this is detected from the prefix alone, before
    /// any payload allocation; the stream is desynchronised afterwards and
    /// must be dropped.
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    Oversized {
        /// The declared or observed frame length in bytes.
        len: usize,
        /// The enforced maximum in bytes.
        max: usize,
    },

    /// The payload was length-correct but rejected by the protocol layer:
    /// invalid JSON, an unknown field, or a violated size limit.
    #[error("frame payload rejected by the protocol")]
    Decode(#[source] ProtocolError),

    /// The peer closed the stream, either cleanly between frames or
    /// mid-frame (a truncated frame). Consumers must treat any in-flight
    /// work as having an unknown outcome (REQ-N-008), never as success.
    #[error("connection closed by peer")]
    Eof,
}

/// Progress of the cancel-safe receive state machine.
#[derive(Debug)]
enum ReadState {
    /// Reading the 4-byte little-endian length prefix.
    Prefix {
        /// Prefix bytes received so far.
        buf: [u8; 4],
        /// How many prefix bytes are valid.
        filled: usize,
    },
    /// Reading a payload whose declared length already passed the size cap.
    Payload {
        /// Payload buffer, allocated at the declared length.
        buf: Vec<u8>,
        /// How many payload bytes are valid.
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

/// Frames [`Message`]s over any async byte stream.
///
/// See the crate documentation for the wire format, cancel-safety, and
/// error-handling contract.
#[derive(Debug)]
pub struct Framed<T> {
    io: T,
    read_state: ReadState,
}

impl<T> Framed<T> {
    /// Wraps a byte stream (or one half of a split stream: sending needs
    /// only [`AsyncWrite`], receiving only [`AsyncRead`]).
    pub fn new(io: T) -> Self {
        Self {
            io,
            read_state: ReadState::start(),
        }
    }

    /// Returns the underlying stream, discarding any partially-read frame.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.io
    }
}

impl<T: AsyncWrite + Unpin> Framed<T> {
    /// Validates, encodes, and writes one frame.
    ///
    /// Not cancel-safe: dropping the returned future mid-write can leave a
    /// partial frame on the stream.
    pub async fn send(&mut self, message: &Message) -> Result<(), TransportError> {
        let payload = message.encode().map_err(map_protocol_error)?;
        self.send_encoded(&payload).await
    }

    /// Writes one frame whose payload was already produced by
    /// [`Message::encode`](cretlink_protocol::Message::encode).
    ///
    /// This exists so callers can validate and encode a message once (for
    /// example before queueing it to a dedicated writer task) while keeping
    /// exactly one implementation of the length-prefix framing. The frame
    /// cap is still enforced here and an oversized payload is rejected
    /// **before any bytes are written**, leaving the stream intact. The
    /// caller is responsible for the payload being a valid encoded
    /// [`Message`]; arbitrary bytes would violate the protocol (though
    /// never the framing bounds).
    ///
    /// Not cancel-safe: dropping the returned future mid-write can leave a
    /// partial frame on the stream.
    pub async fn send_encoded(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        if payload.len() > limits::MAX_FRAME_BYTES {
            return Err(TransportError::Oversized {
                len: payload.len(),
                max: limits::MAX_FRAME_BYTES,
            });
        }
        let len = u32::try_from(payload.len()).map_err(|_| TransportError::Oversized {
            len: payload.len(),
            max: limits::MAX_FRAME_BYTES,
        })?;
        self.io.write_all(&len.to_le_bytes()).await?;
        self.io.write_all(payload).await?;
        self.io.flush().await?;
        Ok(())
    }
}

impl<T: AsyncRead + Unpin> Framed<T> {
    /// Reads one frame and decodes it into a [`Message`].
    ///
    /// The declared length is checked against
    /// [`limits::MAX_FRAME_BYTES`]
    /// before the payload buffer is allocated. Cancel-safe: a dropped
    /// future resumes mid-frame on the next call.
    pub async fn recv(&mut self) -> Result<Message, TransportError> {
        loop {
            match &mut self.read_state {
                ReadState::Prefix { buf, filled } => {
                    if *filled == buf.len() {
                        let len = u32::from_le_bytes(*buf) as usize;
                        if len > limits::MAX_FRAME_BYTES {
                            return Err(TransportError::Oversized {
                                len,
                                max: limits::MAX_FRAME_BYTES,
                            });
                        }
                        self.read_state = ReadState::Payload {
                            buf: vec![0; len],
                            filled: 0,
                        };
                    } else {
                        let read = self.io.read(&mut buf[*filled..]).await?;
                        if read == 0 {
                            return Err(TransportError::Eof);
                        }
                        *filled += read;
                    }
                }
                ReadState::Payload { buf, filled } => {
                    if *filled == buf.len() {
                        let payload = core::mem::take(buf);
                        self.read_state = ReadState::start();
                        return Message::decode(&payload).map_err(map_protocol_error);
                    }
                    let read = self.io.read(&mut buf[*filled..]).await?;
                    if read == 0 {
                        return Err(TransportError::Eof);
                    }
                    *filled += read;
                }
            }
        }
    }
}

/// Maps a protocol failure onto the transport error taxonomy: size-cap
/// violations stay [`TransportError::Oversized`]; everything else is a
/// payload the protocol rejected.
fn map_protocol_error(error: ProtocolError) -> TransportError {
    match error {
        ProtocolError::Oversized { len, max } => TransportError::Oversized { len, max },
        other => TransportError::Decode(other),
    }
}

#[cfg(test)]
mod tests;
