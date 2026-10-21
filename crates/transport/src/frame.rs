//! Minimal length-prefixed frame I/O shared by the pairing handshake and the
//! secure channel (ST-M002-004, ADR-0003).
//!
//! Every frame is a 4-byte little-endian `u32` length prefix followed by
//! exactly that many bytes. This mirrors the plaintext [`crate::Framed`] wire
//! format so the Noise handshake messages and the encrypted transport records
//! share one framing, but the payloads here are **raw Noise messages** (during
//! the handshake) or **Noise ciphertext** (during transport), never plaintext
//! JSON.
//!
//! The declared length is checked against a caller-supplied ceiling **before**
//! the payload buffer is allocated, so a hostile or corrupt length prefix
//! fails closed without committing memory (REQ-N-012 bounded inputs). The
//! ceiling differs by phase: handshake reads bound by
//! [`limits::MAX_FRAME_BYTES`](cretlink_protocol::limits::MAX_FRAME_BYTES);
//! transport reads bound by that plus the Noise tag overhead
//! ([`crate::secure::MAX_CIPHERTEXT_LEN`]).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A low-level framing failure, mapped by callers onto their own error types.
#[derive(Debug)]
pub(crate) enum FrameError {
    /// An I/O failure while reading or writing the stream.
    Io(std::io::Error),
    /// The declared or offered frame length exceeded the ceiling. On a read
    /// this is detected from the length prefix alone, before allocation.
    Oversized {
        /// The declared or offered length in bytes.
        len: usize,
        /// The enforced ceiling in bytes.
        max: usize,
    },
    /// The peer closed the stream, either cleanly at a frame boundary or in the
    /// middle of a frame (a truncated frame). Callers treat any in-flight work
    /// as having an unknown outcome (REQ-N-008), never as success.
    Eof,
}

/// Reads one length-prefixed frame, rejecting an over-`max` declared length
/// from the 4-byte prefix **before** allocating the payload buffer.
///
/// This is not cancel-safe: a dropped future may consume bytes from the stream
/// and leave the next read desynchronised. The pairing handshake and the
/// secure channel drive it with strict turn-taking and poison the session on
/// any error, so a partial read is never resumed.
pub(crate) async fn read_frame<T: AsyncRead + Unpin>(
    io: &mut T,
    max: usize,
) -> Result<Vec<u8>, FrameError> {
    let mut prefix = [0u8; 4];
    read_exact(io, &mut prefix).await?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len > max {
        return Err(FrameError::Oversized { len, max });
    }
    let mut buf = vec![0u8; len];
    read_exact(io, &mut buf).await?;
    Ok(buf)
}

/// Writes one length-prefixed frame, rejecting an over-`max` payload before
/// writing any bytes so the stream is never left with a partial frame.
pub(crate) async fn write_frame<T: AsyncWrite + Unpin>(
    io: &mut T,
    payload: &[u8],
    max: usize,
) -> Result<(), FrameError> {
    if payload.len() > max {
        return Err(FrameError::Oversized {
            len: payload.len(),
            max,
        });
    }
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::Oversized {
        len: payload.len(),
        max,
    })?;
    io.write_all(&len.to_le_bytes())
        .await
        .map_err(FrameError::Io)?;
    io.write_all(payload).await.map_err(FrameError::Io)?;
    io.flush().await.map_err(FrameError::Io)?;
    Ok(())
}

/// Fills `buf` completely, mapping a short read (peer closed) to [`FrameError::Eof`].
async fn read_exact<T: AsyncRead + Unpin>(io: &mut T, buf: &mut [u8]) -> Result<(), FrameError> {
    match io.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(FrameError::Eof),
        Err(e) => Err(FrameError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::MAX_CIPHERTEXT_LEN;
    use cretlink_protocol::limits;
    use tokio::io::{AsyncWriteExt, duplex};

    // Both the handshake read path (bound = MAX_FRAME_BYTES) and the transport
    // read path (bound = MAX_CIPHERTEXT_LEN) reject an oversized declared
    // length from the prefix alone, before allocating a payload buffer. The
    // writer stays open and never sends a payload, so if `read_frame` allocated
    // and waited for the declared bytes it would block here forever.
    async fn rejects_oversized_prefix_before_alloc(max: usize) {
        let (mut client, server) = duplex(64);
        let declared = u32::try_from(max + 1).unwrap();
        client.write_all(&declared.to_le_bytes()).await.unwrap();
        client.flush().await.unwrap();

        let mut server = server;
        let err = read_frame(&mut server, max).await.unwrap_err();
        assert!(matches!(err, FrameError::Oversized { len, max: m } if len == max + 1 && m == max));
        drop(client);
    }

    #[tokio::test]
    async fn handshake_read_rejects_oversized_prefix_before_alloc() {
        rejects_oversized_prefix_before_alloc(limits::MAX_FRAME_BYTES).await;
    }

    #[tokio::test]
    async fn transport_read_rejects_oversized_prefix_before_alloc() {
        rejects_oversized_prefix_before_alloc(MAX_CIPHERTEXT_LEN).await;
    }

    #[tokio::test]
    async fn clean_close_surfaces_as_eof() {
        let (client, server) = duplex(64);
        drop(client);
        let mut server = server;
        assert!(matches!(
            read_frame(&mut server, limits::MAX_FRAME_BYTES)
                .await
                .unwrap_err(),
            FrameError::Eof
        ));
    }

    #[tokio::test]
    async fn write_rejects_oversized_payload_before_writing() {
        let (client, server) = duplex(64);
        let mut client = client;
        let payload = vec![0u8; 8];
        let err = write_frame(&mut client, &payload, 4).await.unwrap_err();
        assert!(matches!(err, FrameError::Oversized { len: 8, max: 4 }));
        drop(client);
        let mut server = server;
        // Nothing reached the wire: the peer sees a clean close, not a frame.
        assert!(matches!(
            read_frame(&mut server, limits::MAX_FRAME_BYTES)
                .await
                .unwrap_err(),
            FrameError::Eof
        ));
    }
}
