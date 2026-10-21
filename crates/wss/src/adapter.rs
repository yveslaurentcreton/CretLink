//! [`WsByteStream`] — a byte-stream view of a WebSocket connection.
//!
//! The CretLink relay carries an **opaque, end-to-end Noise session**: a raw
//! byte stream that must survive transport unchanged (ADR-0016 §5). A WebSocket
//! is message-framed, not byte-oriented, so this adapter re-presents a
//! [`WebSocketStream`] as [`AsyncRead`] + [`AsyncWrite`]. The rendezvous
//! preamble reader, [`cretlink_relay::Rendezvous`], and the session runtime then
//! operate over it exactly as they do over a bare `TcpStream` — the wss framing
//! is invisible above this layer.
//!
//! Fidelity rules that keep the carried stream byte-exact:
//!
//! - **Writes** map each buffer to one binary message. WebSocket message
//!   boundaries are not stream boundaries, so a reader must never assume a read
//!   returns whole writes — it does not, and neither does a TCP socket.
//! - **Reads** concatenate the payloads of inbound binary messages and hand them
//!   out in whatever chunk sizes the caller's buffer allows, buffering any
//!   remainder. Ping/Pong control frames are transparently skipped; a Close (or
//!   the stream ending) is a clean end-of-file.
//! - A **text or raw frame is a protocol violation** for an opaque byte carrier
//!   and surfaces as [`io::ErrorKind::InvalidData`] rather than being silently
//!   reinterpreted.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;

/// Adapts a [`WebSocketStream`] into an [`AsyncRead`] + [`AsyncWrite`] byte
/// stream so a byte-oriented protocol rides over WebSocket framing unchanged.
///
/// `S` is the underlying transport the WebSocket runs on — for the relay that is
/// a TLS stream, but the adapter is generic and imposes nothing on it.
pub struct WsByteStream<S> {
    inner: WebSocketStream<S>,
    /// Bytes received in a binary message but not yet handed to a reader.
    read_rem: Vec<u8>,
    /// Read cursor into `read_rem`.
    read_pos: usize,
    /// Whether the write half has already been closed, so shutdown is idempotent.
    write_closed: bool,
}

impl<S> WsByteStream<S> {
    /// Wraps an established [`WebSocketStream`] as a byte stream.
    #[must_use]
    pub fn new(inner: WebSocketStream<S>) -> Self {
        Self {
            inner,
            read_rem: Vec::new(),
            read_pos: 0,
            write_closed: false,
        }
    }
}

impl<S> fmt::Debug for WsByteStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsByteStream")
            .field(
                "buffered_read_bytes",
                &(self.read_rem.len() - self.read_pos),
            )
            .field("write_closed", &self.write_closed)
            .finish()
    }
}

/// Maps a tungstenite error onto an [`io::Error`], preserving the underlying IO
/// error unchanged and wrapping anything else as [`io::ErrorKind::Other`].
fn ws_err(e: WsError) -> io::Error {
    match e {
        WsError::Io(io) => io,
        other => io::Error::other(other.to_string()),
    }
}

impl<S> AsyncRead for WsByteStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            // Drain any buffered remainder from a previous message first.
            if self.read_pos < self.read_rem.len() {
                let rem = &self.read_rem[self.read_pos..];
                let n = rem.len().min(buf.remaining());
                buf.put_slice(&rem[..n]);
                self.read_pos += n;
                if self.read_pos == self.read_rem.len() {
                    self.read_rem.clear();
                    self.read_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(msg))) => match msg {
                    Message::Binary(data) => {
                        if data.is_empty() {
                            continue;
                        }
                        self.read_rem = data;
                        self.read_pos = 0;
                        continue;
                    }
                    // Control frames carry no stream bytes; skip them.
                    Message::Ping(_) | Message::Pong(_) => continue,
                    // A close, like a half-closed socket, is a clean EOF.
                    Message::Close(_) => return Poll::Ready(Ok(())),
                    // An opaque byte carrier never emits text/raw frames.
                    Message::Text(_) | Message::Frame(_) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "unexpected non-binary websocket message on a byte stream",
                        )));
                    }
                },
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(ws_err(e))),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<S> AsyncWrite for WsByteStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_ready(cx) {
            Poll::Ready(Ok(())) => {
                let msg = Message::Binary(buf.to_vec());
                Pin::new(&mut self.inner).start_send(msg).map_err(ws_err)?;
                Poll::Ready(Ok(buf.len()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(ws_err(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx).map_err(ws_err)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.write_closed {
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut self.inner).poll_close(cx) {
            Poll::Ready(Ok(())) => {
                self.write_closed = true;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(ws_err(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}
