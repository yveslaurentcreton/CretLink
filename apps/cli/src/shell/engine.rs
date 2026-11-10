//! The transport-abstract interactive-terminal engine (ST-M008-002).
//!
//! This module is the whole security-relevant path of the `cretlink-cli shell`
//! client, written against the async [`tokio::io`] traits rather than a real
//! terminal so that fail-closed routing, honest stream termination, and the
//! guarantee that only caller-supplied input is ever forwarded can all be
//! exercised headlessly (see the [`super`] module docs and
//! `tests/shell_engine.rs`).
//!
//! The client is a **third** same-user local IPC consumer beside the MCP bridge
//! and the console's own panel. It holds no session authority: it performs the
//! [`cretlink_mcp::ipc`] handshake, and — only when the console reports a
//! consented active session — opens and attaches to an interactive terminal
//! over the existing versioned wire ops ([`IpcRequest::TermOpen`] /
//! `TermAttach` / `TermInput` / `TermResize` / `TermDetach` / `TermClose`, and
//! the server-pushed [`TermFrame`] duplex). It adds no new listener, endpoint,
//! protocol variant, or trust anchor (REQ-S-023, REQ-N-012). With no consented
//! session, or after a revoke or stop, every call fails closed and nothing runs
//! (REQ-S-009), and the run ends on an honest [`RunEnd`] rather than a
//! fabricated clean close (REQ-N-008).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cretlink_mcp::ipc::{
    IPC_PROTOCOL, IpcError, IpcRequest, IpcResponse, TermFrame, read_message, write_message,
};
use cretlink_protocol::limits::{MAX_TERM_COLS, MAX_TERM_ROWS, MIN_TERM_COLS, MIN_TERM_ROWS};
use cretlink_protocol::{
    B64Bytes, JobId, JobOutcome, ProtocolVersion, SessionId, Shell, TargetIdentity,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// The lowest IPC contract minor that carries the interactive-terminal wire ops
/// (ADR-0013, ST-M003-009). A console older than this speaks a compatible major
/// but cannot serve a terminal, so the client refuses rather than sending a
/// `TermOpen` the console would reject field-by-field.
pub const TERMINAL_MIN_MINOR: u16 = 4;

/// How a caller opens the interactive terminal on the console's routed session.
///
/// These are exactly the fields of [`IpcRequest::TermOpen`]; the engine forwards
/// them verbatim and never substitutes a default shell or a local path.
#[derive(Debug, Clone)]
pub struct OpenConfig {
    /// Which shell runs in the terminal.
    pub shell: Shell,
    /// Initial terminal width in columns (clamped to the protocol bounds).
    pub cols: u16,
    /// Initial terminal height in rows (clamped to the protocol bounds).
    pub rows: u16,
    /// Optional remote working directory (validated by the routed session).
    pub working_dir: Option<String>,
    /// Extra remote environment overrides.
    pub env: BTreeMap<String, String>,
}

/// The routed identity and terminal handle returned by a successful attach.
///
/// It always carries the console-reported `session` and `target` so the host can
/// show the technician *which* consented session and machine the terminal is
/// bound to before any keystroke is forwarded (REQ-F-016).
#[derive(Debug, Clone)]
pub struct Attached {
    /// The routed session the terminal belongs to.
    pub session: SessionId,
    /// The routed target identity the terminal runs on.
    pub target: TargetIdentity,
    /// The console's negotiated IPC contract version.
    pub console_protocol: ProtocolVersion,
    /// The attached terminal job.
    pub term: JobId,
}

/// A single local input event fed to the running terminal.
///
/// Only values a caller constructs from real local activity are ever turned into
/// wire messages; the engine synthesizes none of its own. `Bytes` are the raw
/// keystrokes, `Resize` follows a host window resize, and `Detach`/`Close` are
/// the two honest ways to end the local side.
#[derive(Debug)]
pub enum InputEvent {
    /// Raw input bytes to feed the terminal's pseudo-console.
    Bytes(Vec<u8>),
    /// A local window resize.
    Resize {
        /// New width in columns (clamped to the protocol bounds).
        cols: u16,
        /// New height in rows (clamped to the protocol bounds).
        rows: u16,
    },
    /// Detach this consumer, leaving the terminal running for others.
    Detach,
    /// Close the terminal for every attached consumer.
    Close,
}

/// How a terminal run ended — always one honest outcome, never a fabricated
/// clean close (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// The terminal job exited; carries the console's honest outcome and code.
    Exited {
        /// The honest terminal outcome.
        outcome: JobOutcome,
        /// The process exit code, when one exists.
        exit_code: Option<i32>,
    },
    /// The session was revoked or stopped while attached (REQ-S-004, REQ-S-009).
    Revoked,
    /// This consumer fell too far behind and was dropped (REQ-N-006); the
    /// terminal and other consumers are unaffected.
    Dropped,
    /// The local side detached cleanly, leaving the terminal running.
    Detached,
    /// The IPC link was lost before an honest end frame arrived.
    LinkLost,
}

/// Why a terminal could not be opened and attached.
///
/// Every variant is fail-closed: nothing on the remote runs when
/// `open_and_attach` returns `Err` (REQ-S-009).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The console reported no consented active session (the fail-closed idle
    /// state). Nothing ran.
    NoConsentedSession,
    /// The console's IPC contract major is incompatible with this client's.
    IncompatibleVersion {
        /// The console's version.
        console: ProtocolVersion,
        /// This client's version.
        client: ProtocolVersion,
    },
    /// The console speaks a compatible major but a minor older than
    /// [`TERMINAL_MIN_MINOR`], so it cannot serve an interactive terminal.
    UnsupportedTerminalMinor {
        /// The console's version.
        console: ProtocolVersion,
    },
    /// The console refused the open or attach with a routed error other than the
    /// fail-closed no-session case (e.g. an unknown terminal or a target error).
    Refused(IpcError),
    /// The IPC link failed or answered outside the contract during the
    /// handshake, open, or attach.
    Link,
}

/// Performs the handshake, then opens and attaches to an interactive terminal on
/// the console's consented session over a single persistent connection.
///
/// On success the connection is in duplex stream mode (the console will push
/// [`TermFrame`]s while the caller keeps sending `Term*` messages); drive it with
/// [`pump`]. On any failure the terminal is not running (REQ-S-009).
///
/// # Errors
///
/// Returns [`SessionError`] when the console is idle (no consented session), the
/// contract is incompatible or too old for a terminal, the console refuses the
/// open/attach, or the link fails.
pub async fn open_and_attach<S>(stream: &mut S, cfg: &OpenConfig) -> Result<Attached, SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // 1. Handshake. Learn the console's contract version and whether a consented
    //    active session exists before anything is opened.
    send(
        stream,
        &IpcRequest::Hello {
            protocol: IPC_PROTOCOL,
        },
    )
    .await?;
    let (console_protocol, session, target) = match recv(stream).await? {
        IpcResponse::Hello {
            protocol,
            session,
            target,
        } => (protocol, session, target),
        IpcResponse::Error { error, .. } => return Err(map_refused(error)),
        _ => return Err(SessionError::Link),
    };
    if !console_protocol.is_compatible_with(IPC_PROTOCOL) {
        return Err(SessionError::IncompatibleVersion {
            console: console_protocol,
            client: IPC_PROTOCOL,
        });
    }
    if console_protocol.minor < TERMINAL_MIN_MINOR {
        return Err(SessionError::UnsupportedTerminalMinor {
            console: console_protocol,
        });
    }
    // Fail closed: no consented session means nothing may run.
    let (session, target) = match (session, target) {
        (Some(session), Some(target)) => (session, target),
        _ => return Err(SessionError::NoConsentedSession),
    };

    // 2. Open the terminal on the routed session.
    send(
        stream,
        &IpcRequest::TermOpen {
            shell: cfg.shell,
            cols: clamp_cols(cfg.cols),
            rows: clamp_rows(cfg.rows),
            working_dir: cfg.working_dir.clone(),
            env: cfg.env.clone(),
        },
    )
    .await?;
    let term = match recv(stream).await? {
        IpcResponse::TermOpened { job, .. } => job,
        IpcResponse::Error { error, .. } => return Err(map_refused(error)),
        _ => return Err(SessionError::Link),
    };

    // 3. Attach as a local consumer; a successful reply enters duplex mode.
    send(stream, &IpcRequest::TermAttach { term }).await?;
    match recv(stream).await? {
        IpcResponse::TermAttached { term: attached, .. } if attached == term => {}
        IpcResponse::TermAttached { .. } => return Err(SessionError::Link),
        IpcResponse::Error { error, .. } => return Err(map_refused(error)),
        _ => return Err(SessionError::Link),
    }

    Ok(Attached {
        session,
        target,
        console_protocol,
        term,
    })
}

/// Runs the bounded duplex terminal pump until the run ends.
///
/// After a successful [`open_and_attach`], this splits the connection and runs
/// two cooperating tasks over it: a reader that forwards [`TermFrame::Output`]
/// chunks to `output` and is authoritative for the server-driven ends
/// (`Exited`/`Revoked`/`Dropped`) and link loss, and a writer that turns each
/// caller [`InputEvent`] into the matching `Term*` message. The first honest end
/// wins; the other task is then torn down and the connection dropped.
///
/// Only events received on `input` are ever forwarded — the engine fabricates no
/// input of its own. A closed `output` receiver (the local host is gone) ends the
/// run as [`RunEnd::LinkLost`].
pub async fn pump<S>(
    stream: S,
    term: JobId,
    mut input: mpsc::Receiver<InputEvent>,
    output: mpsc::UnboundedSender<Vec<u8>>,
) -> RunEnd
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader_half, mut writer_half) = tokio::io::split(stream);
    // The first task to observe an honest end sends it here; the other is torn
    // down. Capacity 2 lets both tasks record without ever blocking.
    let (end_tx, mut end_rx) = mpsc::channel::<RunEnd>(2);
    // Latched by the writer before it sends `TermDetach` so the reader's ensuing
    // EOF is reported as the clean `Detached` end, never as `LinkLost`.
    let detaching = Arc::new(AtomicBool::new(false));

    let reader = {
        let end_tx = end_tx.clone();
        let detaching = Arc::clone(&detaching);
        tokio::spawn(async move {
            loop {
                match read_message::<_, TermFrame>(&mut reader_half).await {
                    Ok(TermFrame::Output {
                        term: frame_term,
                        data,
                        ..
                    }) => {
                        if frame_term != term {
                            continue;
                        }
                        if output.send(data.into_inner()).is_err() {
                            // The local host dropped its receiver; end the run.
                            let _ = end_tx.try_send(RunEnd::LinkLost);
                            break;
                        }
                    }
                    Ok(TermFrame::Exited {
                        outcome, exit_code, ..
                    }) => {
                        let _ = end_tx.try_send(RunEnd::Exited { outcome, exit_code });
                        break;
                    }
                    Ok(TermFrame::Revoked { .. }) => {
                        let _ = end_tx.try_send(RunEnd::Revoked);
                        break;
                    }
                    Ok(TermFrame::Dropped { .. }) => {
                        let _ = end_tx.try_send(RunEnd::Dropped);
                        break;
                    }
                    Err(_) => {
                        // An expected EOF right after the writer latched a detach
                        // is the clean local detach, not a link failure.
                        if !detaching.load(Ordering::SeqCst) {
                            let _ = end_tx.try_send(RunEnd::LinkLost);
                        }
                        break;
                    }
                }
            }
        })
    };

    let writer = {
        let end_tx = end_tx.clone();
        let detaching = Arc::clone(&detaching);
        tokio::spawn(async move {
            loop {
                let event = match input.recv().await {
                    Some(event) => event,
                    // The local input source closed with no explicit close; treat
                    // it as a `Close` so the terminal ends honestly for everyone.
                    None => InputEvent::Close,
                };
                let request = match event {
                    InputEvent::Bytes(bytes) => IpcRequest::TermInput {
                        term,
                        data: B64Bytes::new(bytes),
                    },
                    InputEvent::Resize { cols, rows } => IpcRequest::TermResize {
                        term,
                        cols: clamp_cols(cols),
                        rows: clamp_rows(rows),
                    },
                    InputEvent::Detach => {
                        detaching.store(true, Ordering::SeqCst);
                        let _ =
                            write_message(&mut writer_half, &IpcRequest::TermDetach { term }).await;
                        let _ = end_tx.try_send(RunEnd::Detached);
                        break;
                    }
                    InputEvent::Close => {
                        // Ask the console to close; the reader authors the honest
                        // `Exited`. If even the request cannot be sent, the link
                        // is gone.
                        if write_message(&mut writer_half, &IpcRequest::TermClose { term })
                            .await
                            .is_err()
                        {
                            let _ = end_tx.try_send(RunEnd::LinkLost);
                        }
                        break;
                    }
                };
                if write_message(&mut writer_half, &request).await.is_err() {
                    let _ = end_tx.try_send(RunEnd::LinkLost);
                    break;
                }
            }
        })
    };

    drop(end_tx);
    let end = end_rx.recv().await.unwrap_or(RunEnd::LinkLost);
    // The run is over; drop both halves (and the connection) and let the tasks
    // unwind. Aborting is safe: any honest detach/close write already completed
    // above before its task recorded the end.
    reader.abort();
    writer.abort();
    let _ = reader.await;
    let _ = writer.await;
    end
}

/// Opens, attaches, and then pumps a terminal to completion over `stream`.
///
/// `on_attached` runs once, after a successful attach and before any input is
/// forwarded, so the host can render the routed identity banner (REQ-F-016).
///
/// # Errors
///
/// Returns [`SessionError`] when the terminal could not be opened and attached;
/// the terminal is not running in that case (REQ-S-009).
pub async fn run<S, F>(
    mut stream: S,
    cfg: &OpenConfig,
    input: mpsc::Receiver<InputEvent>,
    output: mpsc::UnboundedSender<Vec<u8>>,
    on_attached: F,
) -> Result<RunEnd, SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: FnOnce(&Attached),
{
    let attached = open_and_attach(&mut stream, cfg).await?;
    on_attached(&attached);
    Ok(pump(stream, attached.term, input, output).await)
}

async fn send<S>(stream: &mut S, request: &IpcRequest) -> Result<(), SessionError>
where
    S: AsyncWrite + Unpin,
{
    write_message(stream, request)
        .await
        .map_err(|_| SessionError::Link)
}

async fn recv<S>(stream: &mut S) -> Result<IpcResponse, SessionError>
where
    S: AsyncRead + Unpin,
{
    read_message(stream).await.map_err(|_| SessionError::Link)
}

/// Maps a routed error onto the fail-closed no-session case where it applies, so
/// an idle console is always reported as [`SessionError::NoConsentedSession`]
/// rather than an opaque refusal.
fn map_refused(error: IpcError) -> SessionError {
    match error {
        IpcError::NoConsentedSession => SessionError::NoConsentedSession,
        other => SessionError::Refused(other),
    }
}

fn clamp_cols(cols: u16) -> u16 {
    cols.clamp(MIN_TERM_COLS, MAX_TERM_COLS)
}

fn clamp_rows(rows: u16) -> u16 {
    rows.clamp(MIN_TERM_ROWS, MAX_TERM_ROWS)
}
