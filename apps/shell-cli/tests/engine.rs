//! Headless engine tests over an in-memory duplex (ST-M008-002).
//!
//! These drive the transport-abstract [`engine`] against a **scripted fake
//! console** on the other end of a [`tokio::io::duplex`] pipe. No real terminal,
//! no real IPC transport — just the async traits — so the whole
//! security-relevant path runs deterministically on every platform:
//!
//! - **fail-closed routing** — an idle console (no consented session), an
//!   incompatible contract major, and a console too old for interactive
//!   terminals each stop the client during the handshake with nothing opened
//!   (REQ-S-009);
//! - **only caller input is ever forwarded** — the happy-path test asserts the
//!   *exact* list of wire requests the engine emits: the engine fabricates no
//!   input of its own and injects no secrets or extra environment (REQ-S-015);
//! - **honest run termination** — a server-pushed `Exited`/`Revoked`/`Dropped`
//!   and a local `Detach` each map to the matching honest [`RunEnd`], never a
//!   fabricated clean close (REQ-N-008).
//!
//! `unwrap`/`expect`/`panic` are allowed here: this is test-only code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;

use cretlink_mcp::ipc::{
    IPC_PROTOCOL, IpcRequest, IpcResponse, TermFrame, read_message, write_message,
};
use cretlink_protocol::{
    B64Bytes, JobId, JobOutcome, ProtocolVersion, SessionId, Shell, TargetIdentity,
};
use cretlink_shell::engine::{self, InputEvent, OpenConfig, RunEnd, SessionError};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;

/// The terminal job id the fake console mints for every opened terminal.
const TERM: JobId = JobId(1);

/// A generous duplex buffer so small control frames never block a writer while
/// the other side is momentarily busy.
const PIPE: usize = 64 * 1024;

fn identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "SIMULATED-TARGET".to_string(),
        os_version: "Simulated".to_string(),
        arch: "x86_64".to_string(),
        elevated: false,
        simulated: true,
        agent_version: "test".to_string(),
    }
}

/// A default open request the tests reuse when the exact config does not matter.
fn open_config() -> OpenConfig {
    OpenConfig {
        shell: Shell::PowerShell,
        cols: 80,
        rows: 24,
        working_dir: None,
        env: BTreeMap::new(),
    }
}

/// How the fake console ends the run once the client has attached.
enum ServerEnd {
    /// Answer the client interactively: echo every `TermInput` back as merged
    /// output (the pty echo), exit on `TermClose`, and stop on `TermDetach`.
    Interactive,
    /// Immediately tear the stream down with an honest `Revoked` frame.
    RevokeImmediately,
    /// Immediately drop this consumer with an honest `Dropped` frame.
    DropImmediately,
}

/// A fake console that replies only during the handshake and then asserts the
/// client sends nothing further — the fail-closed rejection path. Returns every
/// request it observed (which must be exactly the `Hello`).
async fn handshake_only_console(
    mut server: DuplexStream,
    protocol: ProtocolVersion,
    consented: bool,
) -> Vec<IpcRequest> {
    let mut seen = Vec::new();

    let hello = read_message::<_, IpcRequest>(&mut server)
        .await
        .expect("client sends Hello first");
    seen.push(hello);

    let (session, target) = if consented {
        (Some(SessionId::new()), Some(identity()))
    } else {
        (None, None)
    };
    write_message(
        &mut server,
        &IpcResponse::Hello {
            protocol,
            session,
            target,
        },
    )
    .await
    .expect("write Hello reply");

    // The client must reject during the handshake and open nothing: the next
    // read sees the closed connection, never a `TermOpen`.
    let next = read_message::<_, IpcRequest>(&mut server).await;
    assert!(
        next.is_err(),
        "the client must send no request after a fail-closed handshake, got {next:?}",
    );
    seen
}

/// A fake console that completes the handshake with a consented session, opens
/// and attaches a terminal, then ends the run per `end`. Returns every request
/// it observed, so a test can assert the engine forwarded exactly the caller's
/// events and nothing else.
async fn attached_console(mut server: DuplexStream, end: ServerEnd) -> Vec<IpcRequest> {
    let session = SessionId::new();
    let target = identity();
    let mut seen = Vec::new();

    // Handshake: advertise the real contract and a consented active session.
    let hello = read_message::<_, IpcRequest>(&mut server)
        .await
        .expect("client sends Hello first");
    seen.push(hello);
    write_message(
        &mut server,
        &IpcResponse::Hello {
            protocol: IPC_PROTOCOL,
            session: Some(session),
            target: Some(target.clone()),
        },
    )
    .await
    .expect("write Hello reply");

    // Open.
    let open = read_message::<_, IpcRequest>(&mut server)
        .await
        .expect("client opens a terminal");
    seen.push(open);
    write_message(
        &mut server,
        &IpcResponse::TermOpened {
            session,
            target: target.clone(),
            call_id: 0,
            job: TERM,
        },
    )
    .await
    .expect("write TermOpened");

    // Attach — a successful reply enters duplex stream mode.
    let attach = read_message::<_, IpcRequest>(&mut server)
        .await
        .expect("client attaches");
    seen.push(attach);
    write_message(
        &mut server,
        &IpcResponse::TermAttached {
            session,
            target: target.clone(),
            call_id: 0,
            term: TERM,
        },
    )
    .await
    .expect("write TermAttached");

    match end {
        ServerEnd::RevokeImmediately => {
            write_message(
                &mut server,
                &TermFrame::Revoked {
                    session,
                    target: target.clone(),
                    term: TERM,
                },
            )
            .await
            .expect("push Revoked");
            return seen;
        }
        ServerEnd::DropImmediately => {
            write_message(
                &mut server,
                &TermFrame::Dropped {
                    session,
                    target: target.clone(),
                    term: TERM,
                },
            )
            .await
            .expect("push Dropped");
            return seen;
        }
        ServerEnd::Interactive => {}
    }

    // Duplex: echo input, exit on close, stop on detach.
    loop {
        let req = match read_message::<_, IpcRequest>(&mut server).await {
            Ok(req) => req,
            // The client tore the connection down; nothing more to observe.
            Err(_) => break,
        };
        seen.push(req.clone());
        match req {
            IpcRequest::TermInput { term, data } => {
                assert_eq!(term, TERM, "input must target the attached terminal");
                write_message(
                    &mut server,
                    &TermFrame::Output {
                        session,
                        target: target.clone(),
                        term: TERM,
                        data,
                    },
                )
                .await
                .expect("echo output");
            }
            IpcRequest::TermResize { .. } => {}
            IpcRequest::TermDetach { .. } => break,
            IpcRequest::TermClose { .. } => {
                write_message(
                    &mut server,
                    &TermFrame::Exited {
                        session,
                        target: target.clone(),
                        term: TERM,
                        exit_code: Some(0),
                        outcome: JobOutcome::Completed,
                    },
                )
                .await
                .expect("write Exited");
                break;
            }
            other => panic!("unexpected request after attach: {other:?}"),
        }
    }
    seen
}

/// An idle console (no consented active session) fails the client closed with
/// the explicit no-session error, and the client opens nothing (REQ-S-009).
#[tokio::test]
async fn fails_closed_when_no_consented_session() {
    let (mut client, server) = tokio::io::duplex(PIPE);
    let console = tokio::spawn(handshake_only_console(server, IPC_PROTOCOL, false));

    let err = engine::open_and_attach(&mut client, &open_config())
        .await
        .unwrap_err();
    assert_eq!(err, SessionError::NoConsentedSession);
    drop(client);

    assert_eq!(
        console.await.unwrap(),
        vec![IpcRequest::Hello {
            protocol: IPC_PROTOCOL
        }]
    );
}

/// A console whose contract major differs is rejected during the handshake, and
/// nothing is opened (REQ-N-012).
#[tokio::test]
async fn refuses_an_incompatible_contract_major() {
    let (mut client, server) = tokio::io::duplex(PIPE);
    let console_protocol = ProtocolVersion {
        major: IPC_PROTOCOL.major + 1,
        minor: IPC_PROTOCOL.minor,
    };
    let console = tokio::spawn(handshake_only_console(server, console_protocol, true));

    let err = engine::open_and_attach(&mut client, &open_config())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SessionError::IncompatibleVersion {
            console: console_protocol,
            client: IPC_PROTOCOL,
        }
    );
    drop(client);

    assert_eq!(
        console.await.unwrap(),
        vec![IpcRequest::Hello {
            protocol: IPC_PROTOCOL
        }]
    );
}

/// A same-major console whose minor predates the interactive-terminal message
/// set cannot serve a terminal, so the client refuses to attach and opens
/// nothing (ST-M003-009, REQ-N-012).
#[tokio::test]
async fn refuses_a_console_too_old_for_interactive_terminals() {
    let (mut client, server) = tokio::io::duplex(PIPE);
    let console_protocol = ProtocolVersion {
        major: IPC_PROTOCOL.major,
        minor: 3,
    };
    let console = tokio::spawn(handshake_only_console(server, console_protocol, true));

    let err = engine::open_and_attach(&mut client, &open_config())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        SessionError::UnsupportedTerminalMinor {
            console: console_protocol,
        }
    );
    drop(client);

    assert_eq!(
        console.await.unwrap(),
        vec![IpcRequest::Hello {
            protocol: IPC_PROTOCOL
        }]
    );
}

/// The full happy path: the engine forwards **exactly** the caller's events —
/// the handshake, the open (carrying only the caller's own shell, size, working
/// directory, and environment, with no injected secrets), the attach, then only
/// the input, resize, and close the caller fed — and the terminal's echoed
/// output is streamed back verbatim. Any fabricated or extra wire request would
/// fail the exact-sequence assertion (REQ-S-015, REQ-N-005).
#[tokio::test]
async fn forwards_only_caller_events_and_streams_the_echo() {
    let (mut client, server) = tokio::io::duplex(PIPE);
    let console = tokio::spawn(attached_console(server, ServerEnd::Interactive));

    let cfg = OpenConfig {
        shell: Shell::PowerShell,
        cols: 80,
        rows: 24,
        working_dir: Some("C:\\Work".to_string()),
        env: BTreeMap::from([("CRETLINK_TEST".to_string(), "1".to_string())]),
    };
    let attached = engine::open_and_attach(&mut client, &cfg)
        .await
        .expect("attach succeeds");
    assert_eq!(attached.term, TERM);
    assert_eq!(attached.console_protocol, IPC_PROTOCOL);

    let (input_tx, input_rx) = mpsc::channel(16);
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();
    let pump = tokio::spawn(engine::pump(client, attached.term, input_rx, output_tx));

    // Type a line; the pty echoes it back as the merged output stream.
    input_tx
        .send(InputEvent::Bytes(b"echo hi\r\n".to_vec()))
        .await
        .unwrap();
    assert_eq!(output_rx.recv().await.unwrap(), b"echo hi\r\n".to_vec());

    // A resize and a clean close are the only other caller events.
    input_tx
        .send(InputEvent::Resize {
            cols: 120,
            rows: 40,
        })
        .await
        .unwrap();
    input_tx.send(InputEvent::Close).await.unwrap();

    let end = pump.await.unwrap();
    assert_eq!(
        end,
        RunEnd::Exited {
            outcome: JobOutcome::Completed,
            exit_code: Some(0),
        }
    );

    // The engine emitted exactly these requests — no synthetic input, no extra
    // environment, no injected secrets.
    assert_eq!(
        console.await.unwrap(),
        vec![
            IpcRequest::Hello {
                protocol: IPC_PROTOCOL,
            },
            IpcRequest::TermOpen {
                shell: Shell::PowerShell,
                cols: 80,
                rows: 24,
                working_dir: Some("C:\\Work".to_string()),
                env: BTreeMap::from([("CRETLINK_TEST".to_string(), "1".to_string())]),
            },
            IpcRequest::TermAttach { term: TERM },
            IpcRequest::TermInput {
                term: TERM,
                data: B64Bytes::new(b"echo hi\r\n".to_vec()),
            },
            IpcRequest::TermResize {
                term: TERM,
                cols: 120,
                rows: 40,
            },
            IpcRequest::TermClose { term: TERM },
        ]
    );
}

/// A revoke while attached ends the run with the honest `Revoked` outcome and
/// never a fabricated clean close (REQ-S-004, REQ-S-009, REQ-N-008).
#[tokio::test]
async fn a_revoke_ends_the_run_honestly() {
    let (mut client, server) = tokio::io::duplex(PIPE);
    let console = tokio::spawn(attached_console(server, ServerEnd::RevokeImmediately));

    let attached = engine::open_and_attach(&mut client, &open_config())
        .await
        .expect("attach succeeds");

    // Hold the input sender so the writer never turns a closed channel into a
    // `TermClose`; the run must end solely on the server-pushed `Revoked`.
    let (_input_tx, input_rx) = mpsc::channel(16);
    let (output_tx, _output_rx) = mpsc::unbounded_channel();
    let end = engine::pump(client, attached.term, input_rx, output_tx).await;

    assert_eq!(end, RunEnd::Revoked);
    let _ = console.await;
}

/// A slow-consumer drop ends the run with the honest `Dropped` outcome — an
/// honest close of this consumer only (REQ-N-006, REQ-N-008).
#[tokio::test]
async fn a_consumer_drop_ends_the_run_honestly() {
    let (mut client, server) = tokio::io::duplex(PIPE);
    let console = tokio::spawn(attached_console(server, ServerEnd::DropImmediately));

    let attached = engine::open_and_attach(&mut client, &open_config())
        .await
        .expect("attach succeeds");

    let (_input_tx, input_rx) = mpsc::channel(16);
    let (output_tx, _output_rx) = mpsc::unbounded_channel();
    let end = engine::pump(client, attached.term, input_rx, output_tx).await;

    assert_eq!(end, RunEnd::Dropped);
    let _ = console.await;
}

/// A local detach ends the run cleanly as `Detached` — the writer latches the
/// detach so the reader's ensuing EOF is not misreported as a lost link, and the
/// terminal is left running for other consumers (the tmux model).
#[tokio::test]
async fn a_local_detach_ends_the_run_without_a_link_error() {
    let (mut client, server) = tokio::io::duplex(PIPE);
    let console = tokio::spawn(attached_console(server, ServerEnd::Interactive));

    let attached = engine::open_and_attach(&mut client, &open_config())
        .await
        .expect("attach succeeds");

    let (input_tx, input_rx) = mpsc::channel(16);
    let (output_tx, _output_rx) = mpsc::unbounded_channel();
    let pump = tokio::spawn(engine::pump(client, attached.term, input_rx, output_tx));

    input_tx.send(InputEvent::Detach).await.unwrap();
    let end = pump.await.unwrap();
    assert_eq!(end, RunEnd::Detached);

    assert_eq!(
        console.await.unwrap(),
        vec![
            IpcRequest::Hello {
                protocol: IPC_PROTOCOL,
            },
            IpcRequest::TermOpen {
                shell: Shell::PowerShell,
                cols: 80,
                rows: 24,
                working_dir: None,
                env: BTreeMap::new(),
            },
            IpcRequest::TermAttach { term: TERM },
            IpcRequest::TermDetach { term: TERM },
        ]
    );
}
