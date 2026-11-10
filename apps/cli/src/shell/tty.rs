//! The thin real-terminal host that wires the engine to a live stdin/stdout.
//!
//! This is the untested I/O edge of the terminal client: it carries no protocol
//! logic and no session authority. It puts the host console into raw mode (so
//! keystrokes stream to the remote pseudo-console and its VT output renders
//! locally), streams local stdin to the engine as [`InputEvent`]s, and writes the
//! terminal's merged output back to local stdout. Its raw-mode setup and
//! interactive ergonomics are validated on real terminals as a maintainer
//! hardware task (see `docs/manual-tests.md`); the security-relevant duplex lives
//! in [`super::engine`] and is tested headlessly.

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::{RawMode, terminal_size};

#[cfg(not(windows))]
pub use portable::{RawMode, terminal_size};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::engine::InputEvent;

/// The local hotkey byte that detaches this client, leaving the remote terminal
/// running for other consumers: `Ctrl-]` (`0x1d`), the classic telnet/SSH escape.
/// It is consumed locally and never forwarded to the remote pseudo-console.
pub const DETACH_BYTE: u8 = 0x1d;

/// Streams local stdin to the engine as [`InputEvent`]s until end-of-input or the
/// detach hotkey.
///
/// Raw keystroke bytes are forwarded verbatim as [`InputEvent::Bytes`]; only the
/// caller's real input is ever sent (no synthetic input). Seeing [`DETACH_BYTE`]
/// forwards any bytes before it, then sends [`InputEvent::Detach`] and stops.
/// End-of-input drops the sender, which the engine treats as a clean close.
pub async fn stdin_to_input(tx: mpsc::Sender<InputEvent>) {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        let read = match stdin.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let chunk = &buf[..read];
        if let Some(pos) = chunk.iter().position(|&byte| byte == DETACH_BYTE) {
            if pos > 0
                && tx
                    .send(InputEvent::Bytes(chunk[..pos].to_vec()))
                    .await
                    .is_err()
            {
                break;
            }
            let _ = tx.send(InputEvent::Detach).await;
            break;
        }
        if tx.send(InputEvent::Bytes(chunk.to_vec())).await.is_err() {
            break;
        }
    }
}

/// Writes the terminal's merged output to local stdout as it arrives, flushing
/// each chunk so output is not withheld behind a buffer (REQ-N-005).
pub async fn output_to_stdout(mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    let mut stdout = tokio::io::stdout();
    while let Some(chunk) = rx.recv().await {
        if stdout.write_all(&chunk).await.is_err() || stdout.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(not(windows))]
mod portable {
    //! The non-Windows fallback host. CretLink's targets and the technician
    //! console are Windows-first; a real POSIX raw-mode host is a later
    //! maintainer task, so this fallback compiles and runs without changing the
    //! console mode (line-buffered input, no local VT translation).

    use std::io;

    /// A no-op mode guard on non-Windows hosts. It changes nothing and restores
    /// nothing, so the security-relevant engine can still be built and exercised
    /// on every platform.
    #[derive(Debug)]
    pub struct RawMode;

    impl RawMode {
        /// Returns a no-op guard; the host console mode is left unchanged.
        ///
        /// # Errors
        ///
        /// Never fails on this platform.
        pub fn enable() -> io::Result<Self> {
            Ok(Self)
        }
    }

    /// The host terminal size is not queried on this platform.
    #[must_use]
    pub fn terminal_size() -> Option<(u16, u16)> {
        None
    }
}
