//! CretLink Console command-line entry point (ST-M002-002 wiring).
//!
//! The console is the listening side of an attended LAN session (ADR-0003: the
//! agent dials, the console listens). This binary hosts one session with the
//! real [`cretlink_session`] runtime: it binds a private-interface listener
//! (never `0.0.0.0`, REQ-S-010), advertises itself over mDNS so the customer
//! agent can discover it, prints the address and a manual connection code, and
//! then reports every lifecycle event — the Session Approval Code to compare,
//! consent, the active session, and revocation — as plain, secret-free text.
//!
//! The graphical console (`apps/app`, Give help) renders the same runtime; this
//! CLI is the headless technician console (interactive commands, audit, MCP
//! bridge host).

use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use cretlink_console::{
    AUDIT_CHANNEL_CAPACITY, AuditEntry, AuditServiceConfig, ChannelAudit, ConsoleRoute,
    ConsoleSessionService, IpcAudit, IpcServer, ManualActivity, SessionAuditStart,
    open_session_log, prune_audit_dir, report_prune, spawn_audit_service,
};
use cretlink_discovery::{Advertiser, ConnectionCode, private_interface_addrs};
use cretlink_mcp::{IpcEndpoint, IpcListener};
use cretlink_protocol::{PROTOCOL_V1, SessionId, TargetIdentity};
use cretlink_session::{
    AdmissionToken, ConsoleHints, DesktopAccess, ExecRequest, HostHandle, JobId, OutputStream,
    RelayDial, RendezvousTicket, RevokeReason, SessionEvent, SessionHost, Shell, WssRelayDial,
};
use tokio::sync::broadcast;
use tokio::sync::oneshot;

const DEFAULT_PORT: u16 = 47141;

/// How long the exiting console waits for the audit drain to seal the session
/// log before giving up with an honest note.
const AUDIT_SEAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the cosmetic live AI-activity echo channel. Small and lossy on
/// purpose: a burst that outruns the CLI renderer drops `[ai]` lines rather
/// than growing unboundedly (REQ-N-006). The durable audit log records every
/// entry regardless.
const LIVE_ECHO_CAPACITY: usize = 256;

#[tokio::main]
async fn main() -> ExitCode {
    let code = match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    };
    pause_before_exit();
    code
}

/// Keeps the window open so the technician can read the final outcome (a bind
/// error, or the closing status of a session) when the console was launched in
/// its own window that would otherwise close on exit. When stdin is not a
/// console (piped/redirected) the read returns immediately.
fn pause_before_exit() {
    use std::io::{BufRead, Write};
    print!("\nPress Enter to close this window...");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
}

async fn run() -> Result<(), String> {
    let options = Options::parse(std::env::args().skip(1))?;
    // Resolve the transport up front so a malformed connection request fails
    // closed before any audit or listener state is created (REQ-N-008): a LAN
    // bind address, or a relay rendezvous (address + single-use ticket + optional
    // admission token). Selecting the relay is a transport choice, not a
    // session-logic change (ADR-0016 §5).
    let connection = resolve_connection(options)?;

    // Unified per-session audit (ST-M003-006, ADR-0008): retention pruning
    // runs at console start, then one bounded channel feeds one drain that
    // owns the session log. Session lifecycle (via `host_with_audit`), MCP
    // bridge calls, and manual console commands all land in the same
    // hash-chained file.
    let audit_config = AuditServiceConfig::default();
    report_prune(prune_audit_dir(
        &audit_config.dir,
        &audit_config.retention,
        None,
    ));
    println!("Audit directory: {}", audit_config.dir.display());
    let (audit, audit_rx) = ChannelAudit::bounded(AUDIT_CHANNEL_CAPACITY);
    let (start_tx, start_rx) = oneshot::channel();
    let audit_task = spawn_audit_service(audit_config.clone(), audit_rx, start_rx);

    let result = run_session(connection, &audit_config, audit.clone(), start_tx).await;

    // Release this function's audit sender and give the drain a bounded
    // window to seal the log; an unsealed log stays verifiable but reports
    // `Unsealed`, so say so rather than exit silently.
    drop(audit);
    if tokio::time::timeout(AUDIT_SEAL_TIMEOUT, audit_task)
        .await
        .is_err()
    {
        eprintln!(
            "note: the audit log could not be sealed before exit; it will verify as unsealed"
        );
    }
    result
}

/// Hosts one session end to end: pre-active lifecycle reporting, then the
/// interactive command loop plus the MCP bridge endpoint.
async fn run_session(
    connection: Connection,
    audit_config: &AuditServiceConfig,
    audit: ChannelAudit,
    start_tx: oneshot::Sender<SessionAuditStart>,
) -> Result<(), String> {
    let hints = ConsoleHints {
        label: "This computer (technician)".to_string(),
    };
    // Open the one session's transport. On the LAN the console binds a private
    // listener and advertises itself over mDNS; over the relay it dials outbound
    // and writes its one-shot rendezvous preamble, after which the relay splices
    // the agent's leg onto this one (ADR-0016 §5). Either way the runtime adopts
    // one opaque, end-to-end-encrypted stream — pairing, the Session Approval
    // Code, consent, attribution, revocation, and cleanup are byte-identical
    // (REQ-F-005, REQ-F-016..019). The mDNS advertiser guard (LAN only) is held
    // for the whole session lifetime so the console stays discoverable.
    let (mut handle, _advertiser) = match connection {
        Connection::Lan { bind_addr } => {
            let handle = SessionHost::host_with_audit(
                bind_addr,
                hints,
                PROTOCOL_V1,
                Vec::new(),
                Arc::new(audit.clone()),
            )
            .await
            .map_err(|error| format!("could not start the console listener: {error}"))?;
            let advertiser = announce_lan(&handle)?;
            (handle, advertiser)
        }
        Connection::Relay {
            relay,
            ticket,
            token,
            tls,
        } => {
            let has_token = token.is_some();
            let is_wss = tls.is_some();
            // wss and raw TCP dial the same relay and rendezvous identically; they
            // differ only in the transport envelope, and both yield the same
            // transport-agnostic `HostHandle`. The wss trust store is assembled
            // (and validated) here, before the outbound dial, so a malformed or
            // empty trust store fails closed with no connection (REQ-S-020).
            let handle = match tls {
                Some(tls) => {
                    let dialer = WssRelayDial::with_trust(
                        relay,
                        tls.server_name,
                        tls.ca_pem.as_deref(),
                        ticket,
                        token,
                    )
                    .map_err(|error| {
                        format!("could not establish the relay TLS trust store: {error}")
                    })?;
                    SessionHost::host_dialed(
                        dialer,
                        hints,
                        PROTOCOL_V1,
                        Vec::new(),
                        Arc::new(audit.clone()),
                    )
                    .await
                    .map_err(|error| format!("could not reach the relay: {error}"))?
                }
                None => SessionHost::host_dialed(
                    RelayDial::new(relay, ticket, token),
                    hints,
                    PROTOCOL_V1,
                    Vec::new(),
                    Arc::new(audit.clone()),
                )
                .await
                .map_err(|error| format!("could not reach the relay: {error}"))?,
            };
            announce_relay(relay, has_token, is_wss);
            (handle, None)
        }
    };

    // Pre-active phase: report pairing/consent progress and let Ctrl+C revoke.
    // Once the session is active, hand off to the interactive command loop and
    // the MCP bridge IPC server, which both drive the one session concurrently.
    // The per-session log is opened the moment the runtime adopts a session id
    // — before consent — so a failure to open it can still be answered with an
    // explicit technician choice instead of granting unrecorded control
    // (ADR-0008); lifecycle records queue in the bounded channel until then.
    let mut identity: Option<TargetIdentity> = None;
    let mut start_tx = Some(start_tx);
    loop {
        tokio::select! {
            event = handle.next_event() => {
                match event {
                    Some(event) => {
                        // Capture the consent-bound identity as it arrives, so the
                        // session service can label results before Active (REQ-F-016).
                        if let SessionEvent::ConsentGranted { identity: bound } = &event {
                            identity = Some(bound.clone());
                        }
                        if start_tx.is_some()
                            && let Some(session) = handle.session()
                            && let Some(tx) = start_tx.take()
                        {
                            match open_session_log(audit_config, session) {
                                Ok(start) => {
                                    let _ = tx.send(start);
                                }
                                Err(error) => {
                                    // ADR-0008: continuing without audit is an
                                    // explicit technician choice; the default —
                                    // and the only non-interactive answer — is
                                    // to fail the session closed.
                                    if !confirm_continue_without_audit(&error).await {
                                        println!(
                                            "Failing closed: revoking the session (no audit, no control)."
                                        );
                                        let _ = handle.revoke(RevokeReason::TechnicianRequested);
                                        while let Some(event) = handle.next_event().await {
                                            if report_event(&handle, &event) == Flow::Stop {
                                                break;
                                            }
                                        }
                                        return Err(
                                            "session refused: the audit log could not be created \
                                             and continuing without audit was not confirmed"
                                                .to_string(),
                                        );
                                    }
                                    println!(
                                        "Continuing WITHOUT an audit log at your explicit request \
                                         (ADR-0008). Nothing from this session will be durably recorded."
                                    );
                                    // `tx` is dropped: the drain discards with an honest note.
                                }
                            }
                        }
                        match report_event(&handle, &event) {
                            Flow::Active => return run_active(handle, identity.take(), audit).await,
                            Flow::Stop => return Ok(()),
                            Flow::Compare => {
                                // REQ-S-006 / ST-M002-013: hold the session at the
                                // compare gate until the technician positively
                                // confirms the codes match. A confirmation releases
                                // the gate over the wire; anything else — including a
                                // non-interactive stdin, where nobody can compare —
                                // reports a mismatch and fails the session closed,
                                // so control is never offered on a code we did not
                                // verify out of band.
                                if confirm_codes_match().await {
                                    if let Err(error) = handle.confirm_code() {
                                        let _ = handle.revoke(RevokeReason::TechnicianRequested);
                                        return Err(format!(
                                            "session refused: could not confirm the approval code ({error})"
                                        ));
                                    }
                                } else {
                                    println!(
                                        "Approval codes do NOT match — failing closed (revoking, no control)."
                                    );
                                    let _ = handle.report_code_mismatch();
                                    while let Some(event) = handle.next_event().await {
                                        if report_event(&handle, &event) == Flow::Stop {
                                            break;
                                        }
                                    }
                                    return Err(
                                        "session refused: the approval codes did not match".to_string(),
                                    );
                                }
                            }
                            Flow::Continue => {}
                        }
                    }
                    None => {
                        println!("Session ended.");
                        return Ok(());
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nStopping — revoking the session.");
                let _ = handle.revoke(RevokeReason::TechnicianRequested);
                // Drain remaining events so the outcome is reported honestly.
                while let Some(event) = handle.next_event().await {
                    if report_event(&handle, &event) == Flow::Stop {
                        break;
                    }
                }
                return Ok(());
            }
        }
    }
}

/// Opens the LAN listening surface: advertises the bound console over mDNS
/// (best effort) and prints the listening address plus the manual connection
/// code, so the customer agent can discover or be pointed at this console
/// (ADR-0007). Returns the advertiser guard, held for the session lifetime, or
/// `None` when multicast is unavailable — the manual code still works.
///
/// The listener always reports a bound address here; a missing one fails closed
/// rather than advertising a fabricated endpoint (REQ-N-008).
fn announce_lan(handle: &HostHandle) -> Result<Option<Advertiser>, String> {
    let local_addr = handle
        .local_addr()
        .ok_or_else(|| "the console listener reported no local address".to_string())?;
    let advertiser = match Advertiser::advertise(
        "cretlink-console",
        local_addr.port(),
        PROTOCOL_V1,
        "CretLink Console",
    ) {
        Ok(advertiser) => Some(advertiser),
        Err(error) => {
            eprintln!("note: mDNS advertising unavailable ({error}); use the manual code below");
            None
        }
    };
    println!("CretLink Console — waiting for the customer agent to connect.");
    println!("  Listening on: {local_addr}");
    if let SocketAddr::V4(v4) = local_addr {
        println!("  Manual connection code: {}", ConnectionCode::encode(v4));
    }
    println!("  Give the customer the address or code above, then start CretLink Support there.");
    println!("  Press Ctrl+C to stop and revoke at any time.\n");
    Ok(advertiser)
}

/// Opens the relay rendezvous surface (ADR-0016 §5): the console has already
/// dialled the relay outbound and written its rendezvous preamble; it now waits
/// for the relay to splice the agent's matching leg. There is no LAN listener
/// and no mDNS here — the single-use ticket, shared out of band, is what pairs
/// the two endpoints, so it is never printed (REQ-S-017). Only non-secret
/// routing metadata (the relay address and whether an admission token was
/// presented) is shown.
fn announce_relay(relay: SocketAddr, has_token: bool, is_wss: bool) {
    println!("CretLink Console — connected to the relay, waiting for the customer agent.");
    println!("  Relay: {relay}");
    println!(
        "  Transport: {}",
        if is_wss {
            "wss:// (WebSocket over TLS; the relay certificate was validated)"
        } else {
            "raw TCP (no transport encryption; the session stays end-to-end encrypted)"
        }
    );
    println!(
        "  Admission token: {}",
        if has_token {
            "presented"
        } else {
            "none (open relay)"
        }
    );
    println!("  The customer must start CretLink Support with the SAME rendezvous ticket.");
    println!("  Press Ctrl+C to stop and revoke at any time.\n");
}

/// Asks the technician whether the session may continue **without** a durable
/// audit log (ADR-0008: an explicit choice, never a silent degradation).
///
/// Defaults to NO: only a literal interactive `y`/`yes` continues. When stdin
/// is not a terminal there is nobody to ask, so the answer is fail-closed.
async fn confirm_continue_without_audit(error: &str) -> bool {
    eprintln!(
        "\n==================== AUDIT UNAVAILABLE ====================\n\
         The session audit log could not be created:\n  {error}\n\
         Remote control without a durable audit record violates the\n\
         default policy (ADR-0008, REQ-S-017).\n\
         ==========================================================="
    );
    if !std::io::stdin().is_terminal() {
        eprintln!("stdin is not interactive; failing closed without audit.");
        return false;
    }
    print!("Continue WITHOUT an audit log? This is not recommended. [y/N] ");
    {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    let answer = tokio::task::spawn_blocking(read_console_line)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let answer = answer.trim();
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

/// Asks the technician to positively confirm that the Session Approval Code
/// shown on this console also appears — identically — on the customer's screen
/// (REQ-S-006, ST-M002-013). This is the human half of the compare gate: the
/// codes are equal by construction, but the pause forces a deliberate, spoken
/// comparison before any control is offered.
///
/// Defaults to NO: only a literal interactive `y`/`yes` confirms a match. When
/// stdin is not a terminal there is nobody to compare with, so the answer is
/// fail-closed — the caller then reports a mismatch and tears the session down.
async fn confirm_codes_match() -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!("stdin is not interactive; cannot compare the approval code — failing closed.");
        return false;
    }
    print!("Does the SAME Session Approval Code appear on the customer's screen? [y/N] ");
    {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    let answer = tokio::task::spawn_blocking(read_console_line)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let answer = answer.trim();
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
    /// The session paused at the approval-code compare gate (REQ-S-006,
    /// ST-M002-013): the caller must interactively ask the technician to confirm
    /// the codes match — and release or fail the gate — before consent is
    /// offered on the customer's screen.
    Compare,
    /// The session reached full control; switch to the interactive command loop.
    Active,
}

fn report_event(handle: &HostHandle, event: &SessionEvent) -> Flow {
    match event {
        SessionEvent::Connected => {
            if let Some(code) = handle.approval_code() {
                println!("Connected. Session Approval Code:  {code}");
                println!(
                    "  The SAME code appears on the customer's screen — read it out loud together."
                );
            }
            Flow::Continue
        }
        // REQ-S-006 / ST-M002-013: the compare gate. Both peers pause here after
        // the customer's agent requests consent; the technician must positively
        // confirm the codes match before the customer is ever offered a grant.
        // The interactive prompt (and the wire confirm) runs in the async caller,
        // so this arm only announces the gate and hands control back via
        // `Flow::Compare`.
        SessionEvent::AwaitingApprovalCompare => {
            println!(
                "Compare the Session Approval Code with the customer now, out loud, \
                 before they are offered any control."
            );
            Flow::Compare
        }
        SessionEvent::AwaitingConsent => {
            println!("Waiting for the customer to grant full-control consent...");
            Flow::Continue
        }
        SessionEvent::ConsentGranted { identity } => {
            println!("Consent granted by the customer. Connected target:");
            println!(
                "  host={} os={} arch={} elevated={} simulated={} agent={}",
                identity.hostname,
                identity.os_version,
                identity.arch,
                identity.elevated,
                identity.simulated,
                identity.agent_version,
            );
            Flow::Continue
        }
        SessionEvent::Active => Flow::Active,
        SessionEvent::Revoked { reason } => {
            println!("Session revoked ({reason:?}). Access has been torn down.");
            Flow::Stop
        }
        SessionEvent::Failed { stage, message } => {
            println!("Session ended at {stage}: {message}");
            Flow::Stop
        }
        // These only occur inside the interactive loop (handled there); a pre-active
        // sighting is impossible but keeps this match exhaustive. Reboot-resume can
        // only be armed after consent, so its reflections belong to the same group.
        // `RequestHandled` is agent-side only and never reaches the console.
        SessionEvent::CommandStarted { .. }
        | SessionEvent::RequestHandled { .. }
        | SessionEvent::ExecOutput { .. }
        | SessionEvent::ExecOutputDropped { .. }
        | SessionEvent::ExecFinished { .. }
        | SessionEvent::DesktopActivity { .. }
        | SessionEvent::RebootResumeArmed { .. }
        | SessionEvent::RebootResumeDisarmed { .. } => Flow::Continue,
    }
}

/// Builds the session service (moving in the active handle), starts the MCP
/// bridge IPC server alongside the interactive loop, and runs both against the
/// one selected, consented session (ST-M004-001/002). The interactive CLI and
/// any MCP bridge drive the same session concurrently through the service, and
/// both feed the same per-session audit drain with their own attribution
/// (ST-M003-006): `mcp` for bridge calls, `console` for manual commands.
async fn run_active(
    handle: HostHandle,
    identity: Option<TargetIdentity>,
    audit: ChannelAudit,
) -> Result<(), String> {
    let session = handle
        .session()
        .ok_or_else(|| "active session has no session id".to_string())?;
    let target =
        identity.ok_or_else(|| "active session has no consent-bound identity".to_string())?;
    let (service, _driver) = ConsoleSessionService::start(handle, session, target);

    // Live AI-activity echo (REQ-F-017): capture-time bridge records are also
    // rendered as `[ai]` lines the moment a call starts, so the technician
    // sees what the AI is doing in near real time, not only in the log. The
    // channel is bounded and lossy on purpose: this echo is cosmetic, so a
    // burst that outruns the CLI renderer drops lines rather than growing
    // unboundedly (REQ-N-006); the durable audit log stays the complete
    // source of truth.
    let (live_tx, live_rx) = tokio::sync::mpsc::channel(LIVE_ECHO_CAPACITY);
    audit.set_live_sender(live_tx);

    // Best-effort MCP bridge endpoint: an MCP client spawns `cretlink-mcp`,
    // which connects here and routes tool calls to this session (ADR-0004). A
    // failure to bind never blocks interactive control. Routed calls are
    // audited with correlation args (call id, job id, command, paths, digests)
    // into the same session log the lifecycle and manual commands use.
    let endpoint = IpcEndpoint::user_default();
    let ipc_server = match IpcListener::bind(&endpoint) {
        Ok(listener) => {
            println!("\nMCP bridge endpoint ready ('{}').", endpoint.name());
            println!(
                "  Point your MCP client at the `cretlink-mcp` bridge; its tools run on THIS session."
            );
            let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
            let ipc_audit: Arc<dyn IpcAudit> = Arc::new(audit.clone());
            // The IPC server and the CLI share one in-flight counter so the
            // console can show whether the AI is actively driving (ST-M004-006).
            let in_flight = service.activity_counter();
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            Some((
                tokio::spawn(IpcServer::serve(
                    listener,
                    route,
                    ipc_audit,
                    in_flight,
                    shutdown_rx,
                )),
                shutdown_tx,
            ))
        }
        Err(error) => {
            eprintln!(
                "note: could not start the MCP bridge endpoint ({error}); interactive control still works"
            );
            None
        }
    };

    let result = interactive_session(&service, &audit, session, live_rx).await;
    // Stop the bridge endpoint gracefully when the interactive loop ends: the
    // server stops accepting at once but gives in-flight calls a bounded
    // window to record their completion before aborting, then releases every
    // audit sender so the drain can seal the log (fail-closed either way: the
    // route already refuses everything after `service.stop()`).
    service.stop();
    if let Some((task, shutdown_tx)) = ipc_server {
        let _ = shutdown_tx.send(());
        if tokio::time::timeout(
            cretlink_console::SHUTDOWN_GRACE + Duration::from_secs(2),
            task,
        )
        .await
        .is_err()
        {
            eprintln!("note: the MCP bridge endpoint did not stop within its grace window");
        }
    }
    result
}

/// Per-loop interactive state: which job the CLI is waiting on, plus the
/// manual-command bookkeeping and audit producer ([`ManualActivity`]) that
/// tracks which jobs are manual, records their start/finish pairs, and holds
/// the `redact` toggle (REQ-S-018).
struct CliState {
    /// The job the CLI is currently streaming (at most one manual job runs).
    current_job: Option<JobId>,
    /// Manual-command bookkeeping and audit producer (lib code, tested).
    manual: ManualActivity,
}

/// The interactive command loop: after full control, the technician types
/// commands that run on the remote target, with output streamed live and the
/// honest exit code + outcome shown when each job ends (ST-M003-001/002, PART C).
///
/// PowerShell is the default shell; prefix a line with `!cmd ` to use Command
/// Prompt. `jobs` lists tracked jobs, `cancel` cancels the running job,
/// `redact` toggles audit redaction of typed commands, and `quit`/`exit` ends
/// the session. Ctrl+C cancels the running job but never drops the session.
/// Stdin is read on a blocking task so it never contends with the async event
/// stream. Events come from the session service's broadcast, so the MCP bridge
/// and the operator observe the same live stream; activity lines carry an
/// origin prefix — `[console]` for manual jobs, `[ai]` for bridge jobs
/// (REQ-F-017). Manual commands are audited here with actor `console`
/// (ST-M003-006).
async fn interactive_session(
    service: &ConsoleSessionService,
    audit: &ChannelAudit,
    session: SessionId,
    mut live_rx: tokio::sync::mpsc::Receiver<AuditEntry>,
) -> Result<(), String> {
    println!(
        "\nSession active — attended full control. Type a command to run on the remote target."
    );
    println!("  Default shell: PowerShell. Prefix a line with `!cmd ` to use Command Prompt.");
    println!(
        "  `jobs` lists jobs · `cancel` cancels the running job · `quit`/`exit` ends the session."
    );
    println!(
        "  `redact` toggles audit redaction of typed commands (currently OFF: commands are recorded)."
    );
    println!("  Ctrl+C cancels the running job (it does NOT end the session).");

    let mut events = service.subscribe();
    let mut state = CliState {
        current_job: None,
        manual: ManualActivity::new(),
    };
    let mut line_task = tokio::task::spawn_blocking(read_console_line);
    print_prompt(state.current_job, service.ai_in_flight());

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                match state.current_job {
                    Some(job) => {
                        println!("\n^C — cancelling job {job} (the session stays open)...");
                        let cancel = service.cancel_job(job).await;
                        if let Err(error) = &cancel {
                            println!("  Could not cancel job {job}: {error}");
                        }
                        state.manual.cancel_requested(audit, session, job, cancel.is_ok());
                    }
                    None => {
                        println!("\n(no job is running — type `quit` to end the session)");
                        print_prompt(state.current_job, service.ai_in_flight());
                    }
                }
            }
            line = &mut line_task => {
                let line = line.map_err(|_| "console input task failed".to_string())?;
                match line {
                    // EOF on stdin: end the session cleanly rather than spinning.
                    None => {
                        println!("\nInput closed — ending the session.");
                        service.stop();
                        drain_to_end(service, &mut events, &mut state, audit, session).await;
                        return Ok(());
                    }
                    Some(text) => {
                        if run_console_line(service, audit, session, &text, &mut state).await
                            == Flow::Stop
                        {
                            drain_to_end(service, &mut events, &mut state, audit, session).await;
                            return Ok(());
                        }
                        if state.current_job.is_none() {
                            print_prompt(state.current_job, service.ai_in_flight());
                        }
                        line_task = tokio::task::spawn_blocking(read_console_line);
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if report_active_event(service, &event, &mut state, audit, session)
                            == Flow::Stop
                        {
                            return Ok(());
                        }
                    }
                    // The bounded window overran; a live viewer skips and continues.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        println!("\nSession ended.");
                        return Ok(());
                    }
                }
            }
            // Near-real-time AI activity (REQ-F-017): capture-time bridge
            // records are echoed here so the technician sees what the AI does
            // the moment it starts, not only after it finishes.
            live = live_rx.recv() => {
                if let Some(entry) = live {
                    render_ai_activity(&entry);
                }
            }
        }
    }
}

/// Handles one typed command line, auditing manual actions with actor
/// `console` (ST-M003-006). Returns [`Flow::Stop`] when the operator asks to
/// quit (after requesting the stop).
async fn run_console_line(
    service: &ConsoleSessionService,
    audit: &ChannelAudit,
    session: SessionId,
    text: &str,
    state: &mut CliState,
) -> Flow {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Flow::Continue;
    }
    match trimmed {
        "quit" | "exit" => {
            println!("Ending the session.");
            service.stop();
            Flow::Stop
        }
        "jobs" => {
            match service.list_jobs().await {
                Ok(jobs) if jobs.is_empty() => println!("(no jobs are running)"),
                Ok(jobs) => {
                    for summary in jobs {
                        println!(
                            "  job {} running={} — {}",
                            summary.job, summary.running, summary.command_preview
                        );
                    }
                }
                Err(error) => println!("Could not list jobs: {error}"),
            }
            Flow::Continue
        }
        "cancel" => {
            match state.current_job {
                Some(job) => {
                    let cancel = service.cancel_job(job).await;
                    if let Err(error) = &cancel {
                        println!("Could not cancel job {job}: {error}");
                    }
                    state
                        .manual
                        .cancel_requested(audit, session, job, cancel.is_ok());
                }
                None => println!("(no job is running to cancel)"),
            }
            Flow::Continue
        }
        // REQ-S-018: the technician can mark typed commands sensitive; the
        // toggle's current state is always shown so there is no ambiguity
        // about what the audit log will contain.
        "redact" => {
            state.manual.redact_commands = !state.manual.redact_commands;
            if state.manual.redact_commands {
                println!(
                    "Audit redaction is now ON: typed commands are recorded as [REDACTED] markers."
                );
            } else {
                println!("Audit redaction is now OFF: typed commands are recorded verbatim.");
            }
            Flow::Continue
        }
        _ => {
            if state.current_job.is_some() {
                println!("A job is already running — wait for it to finish or `cancel` it first.");
                return Flow::Continue;
            }
            let (shell, command) = match trimmed.strip_prefix("!cmd ") {
                Some(rest) => (Shell::Cmd, rest.to_string()),
                None => (Shell::PowerShell, trimmed.to_string()),
            };
            match service
                .run_command(ExecRequest::new(shell, command.clone()))
                .await
            {
                Ok(started) => {
                    println!(
                        "[console] [job {} started in {}]",
                        started.job, started.working_dir
                    );
                    state
                        .manual
                        .command_started(audit, session, started.job, &command);
                    state.current_job = Some(started.job);
                }
                Err(error) => {
                    println!("Could not run the command: {error}");
                    state
                        .manual
                        .command_refused(audit, session, &command, &error.to_string());
                }
            }
            Flow::Continue
        }
    }
}

/// Renders one active-session event: streams job output to the terminal live
/// and prints the honest outcome when a job ends, with an origin prefix so the
/// technician can attribute each activity line to the AI bridge (`[ai]`) or a
/// manual console command (`[console]`) at a glance (REQ-F-017). Finish events
/// of manual jobs are audited here — bridge jobs are audited by the routed
/// call itself, so nothing is double-recorded. Returns [`Flow::Stop`] on a
/// terminal session event.
fn report_active_event(
    service: &ConsoleSessionService,
    event: &SessionEvent,
    state: &mut CliState,
    audit: &ChannelAudit,
    session: SessionId,
) -> Flow {
    use std::io::Write;
    match event {
        SessionEvent::ExecOutput { stream, bytes, .. } => {
            // Raw remote bytes (VT sequences passed through) written to the
            // matching local stream.
            match stream {
                OutputStream::Stdout => {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(bytes);
                    let _ = out.flush();
                }
                OutputStream::Stderr => {
                    let mut err = std::io::stderr().lock();
                    let _ = err.write_all(bytes);
                    let _ = err.flush();
                }
            }
            Flow::Continue
        }
        SessionEvent::ExecOutputDropped { job, bytes } => {
            // Honest notice: output outpaced the console buffer and was shed to
            // stay bounded (REQ-N-006). Written to stderr so it is not mistaken
            // for command output.
            let origin = job_origin(state, *job);
            let mut err = std::io::stderr().lock();
            let _ = writeln!(
                err,
                "\n[{origin}] [cretlink: {bytes} bytes of output from job {job} were dropped to stay within the console buffer]"
            );
            let _ = err.flush();
            Flow::Continue
        }
        SessionEvent::ExecFinished {
            job,
            exit_code,
            outcome,
        } => {
            let origin = job_origin(state, *job);
            match exit_code {
                Some(code) => {
                    println!("\n[{origin}] [job {job} finished: {outcome:?} (exit code {code})]");
                }
                None => println!("\n[{origin}] [job {job} finished: {outcome:?}]"),
            }
            // Manual jobs carry their finish record here, sharing the start
            // record's job id and a real duration (ST-M003-006); bridge jobs
            // are recorded by their routed call.
            let _ = state
                .manual
                .job_finished(audit, session, *job, *exit_code, *outcome);
            if state.current_job == Some(*job) {
                state.current_job = None;
                print_prompt(state.current_job, service.ai_in_flight());
            }
            Flow::Continue
        }
        SessionEvent::DesktopActivity { access } => {
            // The technician sees the same observation/control state the customer
            // sees, distinguished in words rather than by colour alone (REQ-F-017,
            // REQ-N-013, ST-M005-005). The graphical console renders this from the
            // same signal; the CLI carries it as an honest text line.
            match access {
                DesktopAccess::Observe => {
                    println!(
                        "\n[cretlink] [screen observation active — a screenshot was captured]"
                    );
                }
                DesktopAccess::Control => {
                    println!(
                        "\n[cretlink] [remote input active — controlling the customer desktop]"
                    );
                }
            }
            Flow::Continue
        }
        SessionEvent::Revoked { reason } => {
            println!("\nSession revoked ({reason:?}). Access has been torn down.");
            Flow::Stop
        }
        SessionEvent::Failed { stage, message } => {
            println!("\nSession ended at {stage}: {message}");
            Flow::Stop
        }
        // Other lifecycle events do not recur once active.
        _ => Flow::Continue,
    }
}

/// The origin prefix for a job's activity lines: `console` for jobs this CLI
/// started, `ai` for jobs driven through the MCP bridge (REQ-F-017).
fn job_origin(state: &CliState, job: JobId) -> &'static str {
    if state.manual.is_manual(job) || state.current_job == Some(job) {
        "console"
    } else {
        "ai"
    }
}

/// Renders one live `[ai]` activity line from a bridge call's capture-time
/// audit record, so the technician sees what the AI is doing the moment it
/// starts (REQ-F-017): an exec start with its command preview (already
/// redaction-aware — a sensitive command renders as `[REDACTED]`), and starts
/// plus outcomes of stdin/cancel/fs/transfer/cleanup calls. Exec output and
/// finish lines come from the session event stream; read-only inspection
/// calls are not echoed to keep the terminal readable — their full detail is
/// in the audit log.
fn render_ai_activity(entry: &AuditEntry) {
    let line = match entry.action {
        "mcp_remote_exec_start" => Some(format!("exec started: {}", arg_preview(entry, "command"))),
        "mcp_remote_write_stdin_start" => {
            Some(format!("stdin -> job {}", arg_preview(entry, "job_id")))
        }
        "mcp_remote_cancel_start" => Some(format!("cancel job {}", arg_preview(entry, "job_id"))),
        "mcp_remote_make_dir_start" => Some(format!("make-dir {}", arg_preview(entry, "path"))),
        "mcp_remote_move_start" => Some(format!(
            "move {} -> {}",
            arg_preview(entry, "path"),
            arg_preview(entry, "to")
        )),
        "mcp_remote_delete_start" => Some(format!("delete {}", arg_preview(entry, "path"))),
        "mcp_remote_remove_dir_start" => Some(format!("remove-dir {}", arg_preview(entry, "path"))),
        "mcp_remote_upload_start" => Some(format!("upload -> {}", arg_preview(entry, "path"))),
        "mcp_remote_download_start" => Some(format!("download {}", arg_preview(entry, "path"))),
        "mcp_remote_cleanup_start" => Some(format!("cleanup ({})", arg_preview(entry, "trigger"))),
        // Completions of the mutating non-exec calls carry their honest
        // outcome; exec completions are already rendered from the session
        // event stream as `[ai] [job .. finished ..]` lines.
        "mcp_remote_write_stdin"
        | "mcp_remote_cancel"
        | "mcp_remote_make_dir"
        | "mcp_remote_move"
        | "mcp_remote_delete"
        | "mcp_remote_remove_dir"
        | "mcp_remote_upload"
        | "mcp_remote_download"
        | "mcp_remote_cleanup" => Some(format!(
            "{} finished: {:?}",
            entry.action.trim_start_matches("mcp_remote_"),
            entry.outcome
        )),
        _ => None,
    };
    if let Some(line) = line {
        println!("\n[ai] [{line}]");
        // The AI's stated purpose rides the start line's audit record; show it
        // under the real operation, labelled as the AI's claim (REQ-F-026,
        // ST-M009-007). A call from an older bridge carries none.
        // It is bounded to one line of at most 200 characters, so it is shown
        // whole rather than clipped like a command preview.
        if entry.action.ends_with("_start")
            && let Some(purpose) = entry.args.0.get("purpose")
        {
            println!("     stated purpose (from the AI): {purpose}");
        }
    }
}

/// A short, single-line preview of one audit arg for live rendering. Uses the
/// audit value's display form (`[REDACTED]`/`[TRUNCATED]` markers included)
/// and clamps it so a long command or path never floods the terminal.
fn arg_preview(entry: &AuditEntry, key: &str) -> String {
    const PREVIEW_BYTES: usize = 120;
    let Some(value) = entry.args.0.get(key) else {
        return "?".to_string();
    };
    let text = value.to_string().replace(['\r', '\n'], " ");
    if text.len() <= PREVIEW_BYTES {
        return text;
    }
    let mut end = PREVIEW_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Drains lifecycle events until the session's broadcast closes, so a
/// technician-requested stop reaches the agent and reports its honest outcome
/// before the service is dropped.
async fn drain_to_end(
    service: &ConsoleSessionService,
    events: &mut broadcast::Receiver<SessionEvent>,
    state: &mut CliState,
    audit: &ChannelAudit,
    session: SessionId,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                let _ = report_active_event(service, &event, state, audit, session);
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Reads one line from the console on a blocking thread; `None` on EOF or error.
fn read_console_line() -> Option<String> {
    use std::io::BufRead;
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

/// Prints the input prompt when no job is running (output would otherwise be
/// interleaved with a live job's stream). When the AI is actively driving the
/// session through the MCP bridge, the prompt carries a concise `AI active (N)`
/// marker so the technician can distinguish active AI control from an idle
/// bridge connection (ST-M004-006); it reads idle at zero.
fn print_prompt(current_job: Option<JobId>, ai_in_flight: u64) {
    use std::io::Write;
    if current_job.is_none() {
        if ai_in_flight > 0 {
            print!("cretlink [AI active ({ai_in_flight})]> ");
        } else {
            print!("cretlink> ");
        }
        let _ = std::io::stdout().flush();
    }
}

/// Raw parsed command-line options, before the transport is resolved. `port`
/// and `bind` are LAN options; `relay`/`ticket`/`token` select and configure the
/// relay rendezvous. Which set is legal together is decided in
/// [`resolve_connection`], which fails closed on a contradictory mix.
struct Options {
    /// An explicit LAN listen port; `None` uses [`DEFAULT_PORT`]. LAN only.
    port: Option<u16>,
    /// An explicit LAN bind address; `None` auto-selects a private interface.
    /// LAN only.
    bind: Option<IpAddr>,
    /// The relay's reachable address; its presence selects relay transport.
    relay: Option<String>,
    /// The single-use rendezvous ticket (hex) naming this pending pairing.
    ticket: Option<String>,
    /// An optional relay admission credential (abuse control only, ADR-0016 §4).
    token: Option<String>,
    /// The DNS name the relay's TLS certificate is validated against; its
    /// presence selects the wss transport (ST-M007-009). Relay only.
    relay_tls: Option<String>,
    /// An optional operator-pinned CA/certificate PEM path; absent selects the OS
    /// root store. Only meaningful with `--relay-tls`. Relay only.
    relay_ca: Option<String>,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut port = None;
        let mut bind = None;
        let mut relay = None;
        let mut ticket = None;
        let mut token = None;
        let mut relay_tls = None;
        let mut relay_ca = None;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--port" | "-p" => {
                    let value = args.next().ok_or("--port needs a value")?;
                    port = Some(
                        value
                            .parse()
                            .map_err(|_| format!("invalid port: {value}"))?,
                    );
                }
                "--bind" | "-b" => {
                    let value = args.next().ok_or("--bind needs a value")?;
                    bind = Some(
                        value
                            .parse()
                            .map_err(|_| format!("invalid bind address: {value}"))?,
                    );
                }
                "--relay" => {
                    relay = Some(args.next().ok_or("--relay needs a value")?);
                }
                "--ticket" => {
                    ticket = Some(args.next().ok_or("--ticket needs a value")?);
                }
                "--token" => {
                    token = Some(args.next().ok_or("--token needs a value")?);
                }
                "--relay-tls" => {
                    relay_tls = Some(args.next().ok_or("--relay-tls needs a value")?);
                }
                "--relay-ca" => {
                    relay_ca = Some(args.next().ok_or("--relay-ca needs a value")?);
                }
                "--help" | "-h" => {
                    return Err(
                        "usage: cretlink-console [--port <port>] [--bind <private-ip>]\n       \
                         cretlink-console --relay <host:port> --ticket <hex> [--token <secret>]\n       \
                         \x20                 [--relay-tls <server-name> [--relay-ca <ca.pem>]]"
                            .to_string(),
                    );
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(Self {
            port,
            bind,
            relay,
            ticket,
            token,
            relay_tls,
            relay_ca,
        })
    }
}

/// The resolved transport the console opens: either a direct LAN listener on a
/// bound private address, or an outbound relay rendezvous carrying the same
/// end-to-end session (ADR-0016 §5). The relay is a transport choice only; every
/// downstream session step is identical (REQ-F-005).
enum Connection {
    /// Bind and listen on this private-interface address (the historical path).
    Lan {
        /// The address to bind (`select_bind_ip` + port).
        bind_addr: SocketAddr,
    },
    /// Dial the relay outbound and rendezvous on `ticket` (ADR-0016 §5).
    Relay {
        /// The relay's reachable address, dialled outbound by both endpoints.
        relay: SocketAddr,
        /// The single-use ticket the relay matches the two legs on.
        ticket: RendezvousTicket,
        /// The optional shared admission credential — never session authority.
        token: Option<AdmissionToken>,
        /// wss transport parameters when TLS is selected; `None` is the raw-TCP
        /// interim (ST-M007-009, ADR-0016 §3).
        tls: Option<RelayTls>,
    },
}

/// The wss transport parameters for a relay dial: the DNS name to validate the
/// relay certificate against, and an optional operator-pinned CA/certificate.
/// Assembled fail-closed before the outbound dial (ST-M007-009).
struct RelayTls {
    /// The DNS name the relay's certificate is validated against.
    server_name: String,
    /// Operator-pinned CA/certificate PEM bytes; `None` selects the OS root
    /// store (system roots the default, a pinned CA the opt-in).
    ca_pem: Option<Vec<u8>>,
}

/// Environment variable naming the relay address (fallback for `--relay`).
const ENV_RELAY: &str = "CRETLINK_RELAY";
/// Environment variable carrying the rendezvous ticket (fallback for `--ticket`).
const ENV_RELAY_TICKET: &str = "CRETLINK_RELAY_TICKET";
/// Environment variable carrying the admission token (fallback for `--token`).
const ENV_RELAY_TOKEN: &str = "CRETLINK_RELAY_TOKEN";
/// Environment variable naming the relay TLS server name (fallback for
/// `--relay-tls`); its presence selects the wss transport.
const ENV_RELAY_TLS: &str = "CRETLINK_RELAY_TLS";
/// Environment variable naming the pinned relay CA path (fallback for
/// `--relay-ca`).
const ENV_RELAY_CA: &str = "CRETLINK_RELAY_CA";

/// Resolves parsed [`Options`] into one [`Connection`], failing closed on any
/// contradictory or malformed request before touching the network (REQ-N-008,
/// REQ-N-015).
///
/// A relay address (from `--relay` or `CRETLINK_RELAY`) selects the relay path;
/// otherwise the console listens on the LAN. In relay mode the LAN options
/// (`--bind`, `--port`) are rejected as meaningless, a rendezvous ticket is
/// required, and an empty ticket or token is treated as absent. In LAN mode the
/// relay-only options (`--ticket`, `--token`) are rejected. Environment
/// fallbacks are consulted only once the relay path is selected, so a stray
/// `CRETLINK_RELAY_TICKET` never breaks a plain LAN run.
fn resolve_connection(options: Options) -> Result<Connection, String> {
    let relay_arg = options.relay.clone().or_else(|| non_empty_env(ENV_RELAY));
    match relay_arg {
        Some(relay_text) => {
            if options.bind.is_some() {
                return Err(
                    "--bind is a LAN option and cannot be combined with --relay".to_string()
                );
            }
            if options.port.is_some() {
                return Err(
                    "--port is a LAN option and cannot be combined with --relay".to_string()
                );
            }
            let relay = relay_text
                .trim()
                .parse::<SocketAddr>()
                .map_err(|_| format!("invalid relay address (expected host:port): {relay_text}"))?;
            let ticket_text = options
                .ticket
                .clone()
                .or_else(|| non_empty_env(ENV_RELAY_TICKET))
                .ok_or(
                    "a rendezvous ticket is required in relay mode (--ticket or \
                     CRETLINK_RELAY_TICKET)",
                )?;
            let ticket = RendezvousTicket::parse_hex(ticket_text.trim())
                .map_err(|error| format!("invalid rendezvous ticket: {error}"))?;
            let token = options
                .token
                .clone()
                .or_else(|| non_empty_env(ENV_RELAY_TOKEN))
                .map(|text| AdmissionToken::from_text(text.trim()));
            let tls = resolve_relay_tls(&options)?;
            Ok(Connection::Relay {
                relay,
                ticket,
                token,
                tls,
            })
        }
        None => {
            if options.ticket.is_some() {
                return Err("--ticket is only valid together with --relay".to_string());
            }
            if options.token.is_some() {
                return Err("--token is only valid together with --relay".to_string());
            }
            if options.relay_tls.is_some() {
                return Err("--relay-tls is only valid together with --relay".to_string());
            }
            if options.relay_ca.is_some() {
                return Err("--relay-ca is only valid together with --relay".to_string());
            }
            let bind_ip = select_bind_ip(options.bind)?;
            let bind_addr = SocketAddr::new(bind_ip, options.port.unwrap_or(DEFAULT_PORT));
            Ok(Connection::Lan { bind_addr })
        }
    }
}

/// Resolves the optional wss transport parameters for a relay dial (ST-M007-009).
///
/// A server name (from `--relay-tls` or `CRETLINK_RELAY_TLS`) selects wss; its
/// absence keeps the raw-TCP interim. A pinned CA (`--relay-ca` /
/// `CRETLINK_RELAY_CA`) is read here so a missing or unreadable file fails closed
/// before any network activity, and is rejected without a server name — a CA with
/// no name to validate against is a misconfiguration, not a silent fallback
/// (REQ-N-015, REQ-S-020).
fn resolve_relay_tls(options: &Options) -> Result<Option<RelayTls>, String> {
    let server_name = options
        .relay_tls
        .clone()
        .or_else(|| non_empty_env(ENV_RELAY_TLS));
    let ca_path = options
        .relay_ca
        .clone()
        .or_else(|| non_empty_env(ENV_RELAY_CA));
    match server_name {
        Some(server_name) => {
            let server_name = server_name.trim().to_string();
            if server_name.is_empty() {
                return Err("--relay-tls needs a non-empty server name".to_string());
            }
            let ca_pem = match ca_path {
                Some(path) => {
                    let path = path.trim();
                    Some(std::fs::read(path).map_err(|error| {
                        format!("could not read the relay CA at {path}: {error}")
                    })?)
                }
                None => None,
            };
            Ok(Some(RelayTls {
                server_name,
                ca_pem,
            }))
        }
        None => {
            if ca_path.is_some() {
                return Err(
                    "--relay-ca requires --relay-tls <server-name> to validate the certificate against"
                        .to_string(),
                );
            }
            Ok(None)
        }
    }
}

/// Reads an environment variable, treating an unset or whitespace-only value as
/// absent so an empty relay/ticket/token env var never masquerades as a value.
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Chooses which local interface to listen on. An explicit `--bind` wins;
/// otherwise the first private LAN address `private_interface_addrs` returns is
/// used so another machine can reach it, falling back to loopback for a
/// local-only run. That candidate list is preference-ordered by the discovery
/// crate — routable-private (RFC1918 / ULA) first, automatically-assigned
/// link-local (`169.254` / `fe80::`) dropped unless it is the only option — so
/// a stray APIPA interface cannot shadow the real LAN address and force a
/// manual `--bind` (ST-M002-010). The runtime independently refuses
/// any non-private address (REQ-S-010).
fn select_bind_ip(explicit: Option<IpAddr>) -> Result<IpAddr, String> {
    if let Some(ip) = explicit {
        return Ok(ip);
    }
    let candidates = private_interface_addrs().unwrap_or_default();
    if let Some(ip) = candidates.iter().find(|ip| !ip.is_loopback()).copied() {
        return Ok(ip);
    }
    if let Some(ip) = candidates.first().copied() {
        return Ok(ip);
    }
    Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}
