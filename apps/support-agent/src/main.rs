//! The headless `cretlink-support-agent` binary (ST-M002-001 / ST-M002-005
//! wiring), a development and test tool: the release archives ship the
//! application `cretlink`, whose Get help role is the customer's program
//! (ST-M009-010, ADR-0018).
//!
//! The customer-facing agent is the dialing side of an attended LAN session
//! (ADR-0003: the agent is the Noise initiator). This binary reports the
//! process's honest effective identity (REQ-S-011), then either discovers a
//! technician console over mDNS or takes a manual address / connection code,
//! dials it with the real [`cretlink_session`] runtime, shows the Session
//! Approval Code and the full-control disclosure, and asks the customer for
//! one informed consent (REQ-F-006, REQ-S-003). Nothing runs under control
//! before the customer grants it; either side can revoke and clean up.
//!
//! On Windows the agent also offers an attended **local** reboot-resume arming
//! gesture (ADR-0014, ST-M006-001): while a session is active the customer can
//! type `arm` to mint a single-use, expiring, target-bound continuity ticket
//! sealed at rest with DPAPI. Arming grants no control by itself — attended
//! consent still runs after any reconnect — and it is never reachable from the
//! console or the MCP surface (REQ-S-001, REQ-S-003).
//!
//! This binary is the headless stdout agent; `apps/app` (Get help) is the
//! graphical consent window over the same session runtime (it advertises file
//! operations and, on Windows, the terminal, but not the desktop or
//! reboot-resume). Once consent is granted the runtime serves command
//! execution, file operations, and — where this build advertises them — the
//! terminal and the desktop.
//!
//! # Offline residual recovery (ST-M006-003)
//!
//! Invoked as `cretlink-support-agent recover [--remove]` — the same verb, report
//! and exit codes as `cretlink recover [--remove]` and `cretlink-cli recover
//! [--remove]`, which replace it in the product (ST-M009-010) — the agent runs a
//! self-contained, offline pass over the resources it owns on the customer
//! machine — staged file trees and sealed reboot-resume tickets today, with
//! every other ownable kind asserted empty. Without `--remove` it only inspects
//! and prints the residue; with `--remove` it removes what it can and reports
//! honestly what was kept and how to remediate it. The verb opens no network
//! connection, joins no session, and grants no control (REQ-F-019, REQ-N-008).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use cretlink_discovery::{Browser, parse_manual_entry};
use cretlink_protocol::{PROTOCOL_V1, TargetIdentity};
use cretlink_session::{
    ActionOutcome, ActivityKind, AdmissionToken, DesktopAccess, Dial, JoinHandle, NoAudit, Origin,
    RebootResumeArmer, RelayDial, RelayFailure, RendezvousTicket, RevokeReason, SessionError,
    SessionEvent, SessionJoiner, WssRelayDial,
};
use cretlink_support_agent::{
    EffectiveIdentity, TicketStore, current_identity, default_ticket_store_path,
    reboot_resume_arming_disclosure, sysinfo,
};
use tokio::sync::mpsc;

/// The program name the shared customer-side texts carry.
const PROGRAM: &str = "cretlink-support-agent";
/// How long each mDNS browse waits before the agent tries again.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Pause between connection attempts when the console is not reachable yet.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// How long an armed reboot-resume ticket stays redeemable — a short window
/// sized for one reboot and reconnect, never open-ended (ADR-0014).
const REBOOT_RESUME_LIFETIME: Duration = Duration::from_secs(15 * 60);

#[tokio::main]
async fn main() -> ExitCode {
    let identity = current_identity();
    print_identity(&identity);

    // The offline residual-recovery verb (ST-M006-003): inspect, or with
    // `--remove` actually remove, the resources this agent owns on the customer
    // machine. It touches only CretLink's own residue, opens no network
    // connection, joins no session, and grants no control — so it runs to
    // completion and exits here, before any dialing.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("recover") {
        let remove = args.iter().skip(1).any(|arg| arg == "--remove");
        return run_recover(remove).await;
    }

    // A single reader owns the console's stdin for the whole run, so consent,
    // the reboot-resume arming gesture, and the closing pause never contend for
    // it. The channel closes on EOF (piped/redirected stdin).
    let mut stdin_rx = spawn_stdin_lines();

    let code = match run(&identity, &mut stdin_rx).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    };
    pause_before_exit(&mut stdin_rx).await;
    code
}

/// Bridges the blocking console stdin to the async loop: one thread reads whole
/// lines and forwards each to the session loop, which reacts to the customer's
/// typed decisions (consent, and the attended reboot-resume arming gesture)
/// without ever blocking the runtime. On EOF (piped or redirected stdin) the
/// sender drops and the channel closes, which the loop reads as "no more input".
fn spawn_stdin_lines() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break; // the session ended; stop reading
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// Keeps the window open so the customer can read the outcome. The agent is
/// typically launched in its own elevated console window, which would otherwise
/// close the instant the process exits — hiding any message (ST-M002-001: the
/// application stays visibly open). Waits for one more line from the shared
/// stdin reader; when stdin is not a console (piped or redirected) the closed
/// channel returns immediately.
async fn pause_before_exit(stdin_rx: &mut mpsc::UnboundedReceiver<String>) {
    use std::io::Write;
    print!("\nPress Enter to close this window...");
    let _ = std::io::stdout().flush();
    let _ = stdin_rx.recv().await;
}

async fn run(
    identity: &EffectiveIdentity,
    stdin_rx: &mut mpsc::UnboundedReceiver<String>,
) -> Result<(), String> {
    // Before joining any session, reclaim staging trees a prior run left behind
    // — a crash that skipped teardown, or a removal that failed and is retried
    // now (ST-M003-005 AC5b, REQ-F-019). This runs at the shutdown-strength
    // trigger, so it also enforces the "removed on shutdown" policy for the last
    // run's Keep-until-shutdown artifacts. It is best-effort and never blocks a
    // new session. The returned share of the staging base is held for the
    // whole run, so another customer-side process never sweeps this run's
    // staging while it is live (ST-M009-010).
    let _staging_in_use = sweep_stale_staging().await;

    // Reboot-resume is opt-in and platform-gated: on Windows a real DPAPI-backed
    // armer is injected and the capability advertised; elsewhere it stays off
    // (ADR-0014, ST-M006-001).
    let (armer, capabilities, reboot_resume_available) = build_reboot_resume();

    let target_identity = build_target_identity(identity);
    // Resolve the transport before dialing so a malformed connection request
    // fails closed up front (REQ-N-015): LAN discovery/manual entry, or a relay
    // rendezvous (address + single-use ticket + optional admission token).
    // Selecting the relay is a transport choice, not a session-logic change
    // (ADR-0016 §5).
    let connection = parse_agent_connection(std::env::args().skip(1))?;
    let mut handle =
        match connect_with_retries(connection, &target_identity, capabilities, armer).await? {
            Some(handle) => handle,
            None => {
                println!("\nCancelled before a session was established.");
                return Ok(());
            }
        };

    let mut awaiting_consent = false;
    let mut stdin_open = true;

    loop {
        tokio::select! {
            event = handle.next_event() => {
                match event {
                    Some(event) => {
                        if on_event(
                            &mut handle,
                            &event,
                            &mut awaiting_consent,
                            stdin_open,
                            reboot_resume_available,
                        ) == Flow::Stop {
                            return Ok(());
                        }
                    }
                    None => {
                        println!("Session ended.");
                        return Ok(());
                    }
                }
            }
            line = stdin_rx.recv(), if stdin_open => {
                match line {
                    Some(line) => {
                        if on_input(
                            &handle,
                            line.trim(),
                            &mut awaiting_consent,
                            reboot_resume_available,
                        ) == Flow::Stop {
                            return Ok(());
                        }
                    }
                    None => {
                        // stdin closed (EOF/piped). If a consent decision was
                        // pending, the absence of interactive input declines it
                        // — control requires a positive attended decision.
                        stdin_open = false;
                        if awaiting_consent {
                            awaiting_consent = false;
                            let _ = handle.deny_consent("no interactive consent input");
                            println!("No interactive input available; consent declined.");
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nStopping — revoking the session.");
                let _ = handle.revoke(RevokeReason::CustomerRequested);
                while let Some(event) = handle.next_event().await {
                    if on_event(
                        &mut handle,
                        &event,
                        &mut awaiting_consent,
                        stdin_open,
                        reboot_resume_available,
                    ) == Flow::Stop {
                        break;
                    }
                }
                return Ok(());
            }
        }
    }
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Handles one lifecycle event from the session runtime. Consent capture is
/// two-step: this arms `awaiting_consent` and prints the prompt, and the typed
/// answer is handled in [`on_input`] — so the runtime is never blocked on stdin.
fn on_event(
    handle: &mut JoinHandle,
    event: &SessionEvent,
    awaiting_consent: &mut bool,
    stdin_open: bool,
    reboot_resume_available: bool,
) -> Flow {
    use std::io::Write;
    match event {
        SessionEvent::Connected => {
            if let Some(code) = handle.approval_code() {
                println!("\nConnected. Session Approval Code:  {code}");
                println!("  Compare this with the code shown on the technician's console.");
            }
            // What the technician's computer says its user is called: a label
            // it chose, not a verified identity (ST-M009-011).
            if let Some(name) = handle.technician_display_name() {
                println!("  The technician's computer says its user is: {name}");
            }
            Flow::Continue
        }
        SessionEvent::AwaitingApprovalCompare => {
            // The compare gate: no consent is offered until the technician
            // confirms the codes match on their side (REQ-S-006, ST-M002-013).
            println!("  Waiting for the technician to confirm the codes match…");
            Flow::Continue
        }
        SessionEvent::AwaitingConsent => {
            // The runtime has sent the consent request; capture the customer's
            // one informed decision (REQ-F-006, REQ-S-003).
            println!("\nFull-control consent — please read:");
            println!("{}", handle.disclosure().text());
            if stdin_open {
                print!("Type YES to grant full control, or anything else to decline: ");
                let _ = std::io::stdout().flush();
                *awaiting_consent = true;
            } else {
                // No interactive input (piped/redirected stdin): consent cannot
                // be given positively, so decline.
                let _ = handle.deny_consent("no interactive consent input");
                println!(
                    "No interactive input available; consent declined. No control was granted."
                );
            }
            Flow::Continue
        }
        SessionEvent::ConsentGranted { .. } => Flow::Continue,
        SessionEvent::Active => {
            println!("Session active — the technician now has attended full control.");
            println!("  Press Ctrl+C to stop and revoke at any time.");
            if reboot_resume_available {
                println!(
                    "  You can type 'arm' to allow continuing this session across one reboot (single-use)."
                );
            }
            Flow::Continue
        }
        // Visible activity: the customer sees every command run and its honest
        // outcome, and whether the AI or the technician drove it — nothing is
        // hidden (REQ-F-017, REQ-S-002, ST-M004-006).
        SessionEvent::CommandStarted {
            job,
            shell,
            command,
            origin,
            purpose,
        } => {
            let who = match origin {
                Origin::Mcp => "The technician's AI assistant is running",
                // Terminal and console jobs are both technician-driven.
                Origin::Console | Origin::Terminal => "The technician is running",
            };
            println!("{who} [job {job}, {shell:?}]: {command}");
            // The AI's stated purpose is shown beside — never instead of — the
            // real command above, and labelled as the AI's own claim
            // (REQ-F-026, ST-M009-007).
            if let Some(purpose) = purpose {
                println!("  Stated reason (from the AI, not verified): {purpose}");
            }
            Flow::Continue
        }
        SessionEvent::RequestHandled {
            origin,
            purpose,
            kind,
            operation,
            bytes,
            outcome,
            ..
        } => {
            // Every other action on this computer: who did it, the real
            // operation, the honest result, and — for the AI — its stated
            // reason (REQ-F-017, REQ-F-026, ST-M009-007). A file leaving the
            // computer is marked as such, with its size (ST-M009-011,
            // REQ-S-016).
            let leaving = if *kind == ActivityKind::FileSent {
                " [a file is leaving this computer]"
            } else {
                ""
            };
            let size = bytes.map_or_else(String::new, |n| format!(" ({n} bytes)"));
            println!(
                "{}: {operation}{size}{leaving} — {}",
                source_label(*origin),
                outcome_label(*outcome)
            );
            if let Some(purpose) = purpose {
                println!("  Stated reason (from the AI, not verified): {purpose}");
            }
            Flow::Continue
        }
        SessionEvent::ActivityShed { count } => {
            // Never a silent gap (ST-M009-011, REQ-N-008).
            println!("({count} activity item(s) could not be shown here.)");
            Flow::Continue
        }
        SessionEvent::ExecOutput { .. } => {
            // Output is streamed to the technician's console, not echoed here.
            Flow::Continue
        }
        // Output shedding is a console-side buffer concern (REQ-N-006); the agent
        // does not stream output locally, so there is nothing to show here.
        SessionEvent::ExecOutputDropped { .. } => Flow::Continue,
        SessionEvent::ExecFinished {
            job,
            exit_code,
            outcome,
        } => {
            match exit_code {
                Some(code) => println!("Job {job} finished: {outcome:?} (exit code {code})"),
                None => println!("Job {job} finished: {outcome:?}"),
            }
            Flow::Continue
        }
        SessionEvent::DesktopActivity { access } => {
            // The customer always sees, in plain words, when the screen is being
            // observed versus when input is being driven — the two are visibly
            // distinct and never distinguished by colour alone (REQ-F-017 through
            // REQ-F-020, REQ-N-013, ST-M005-005). The graphical accessible
            // indicator is delivered on the customer's own surface; this headless
            // agent carries the same honest signal in text.
            match access {
                DesktopAccess::Observe => {
                    println!("Screen observation is active (a screenshot was taken).");
                }
                DesktopAccess::Control => {
                    println!(
                        "Remote input is active (the technician side is controlling this desktop)."
                    );
                }
            }
            Flow::Continue
        }
        SessionEvent::Revoked { reason } => {
            println!("Session revoked ({reason:?}).");
            report_cleanup(handle);
            Flow::Stop
        }
        SessionEvent::Failed { stage, message } => {
            println!("Session ended at {stage}: {message}");
            report_cleanup(handle);
            Flow::Stop
        }
        SessionEvent::RebootResumeArmed {
            not_after_unix_millis,
            ..
        } => {
            // Arming is attended and visible on the customer machine (ADR-0014):
            // surface it plainly, and be explicit that it grants no control on
            // its own — a reboot never restores an active session.
            println!(
                "Reboot-resume armed until {not_after_unix_millis} (single-use; grants no control by itself)."
            );
            Flow::Continue
        }
        SessionEvent::RebootResumeDisarmed { .. } => {
            println!("Reboot-resume disarmed.");
            Flow::Continue
        }
    }
}

/// Who drove an action, in the customer's words.
fn source_label(origin: Origin) -> &'static str {
    match origin {
        Origin::Mcp => "The technician's AI assistant",
        Origin::Terminal => "The technician (terminal)",
        Origin::Console => "The technician",
    }
}

/// The honest result of an action, in the customer's words (REQ-N-008).
fn outcome_label(outcome: ActionOutcome) -> String {
    match outcome {
        ActionOutcome::InProgress => "in progress".to_string(),
        ActionOutcome::Succeeded => "done".to_string(),
        ActionOutcome::Failed { code } => format!("refused or failed ({code:?})"),
        ActionOutcome::Cancelled => "cancelled by the technician".to_string(),
        ActionOutcome::Unknown => "result unknown".to_string(),
    }
}

/// Handles one line the customer typed. First it completes a pending consent
/// decision; otherwise it interprets the attended reboot-resume arming gesture.
fn on_input(
    handle: &JoinHandle,
    line: &str,
    awaiting_consent: &mut bool,
    reboot_resume_available: bool,
) -> Flow {
    if *awaiting_consent {
        *awaiting_consent = false;
        let answer = line.to_ascii_lowercase();
        if answer == "yes" || answer == "y" {
            match handle.grant_consent() {
                Ok(()) => println!("Consent granted. Establishing the attended session..."),
                Err(error) => {
                    println!("Could not grant consent: {error}");
                    return Flow::Stop;
                }
            }
        } else {
            let _ = handle.deny_consent("declined by the customer");
            println!("Consent declined. No control was granted.");
        }
        return Flow::Continue;
    }

    // The attended local reboot-resume arming gesture (ADR-0014, ST-M006-001).
    match line.to_ascii_lowercase().as_str() {
        "" => {}
        "arm" | "arm-reboot" => {
            if !reboot_resume_available {
                println!("Reboot-resume is not available on this agent.");
            } else if !handle.control_active() {
                println!("Reboot-resume can only be armed during an active, consented session.");
            } else {
                print_reboot_resume_disclosure();
                match handle.arm_reboot_resume(REBOOT_RESUME_LIFETIME) {
                    // The RebootResumeArmed event confirms with the expiry.
                    Ok(()) => {}
                    Err(error) => println!("Could not arm reboot-resume: {error}"),
                }
            }
        }
        _ => {
            if reboot_resume_available && handle.control_active() {
                println!(
                    "Commands: type 'arm' to arm single-use reboot-resume, or Ctrl+C to stop."
                );
            }
        }
    }
    Flow::Continue
}

/// Prints the attended local disclosure shown before arming reboot-resume. The
/// text is the library's single source of truth ([`reboot_resume_arming_disclosure`]),
/// so the disclosed terms cannot drift from the armer's behaviour (ADR-0014).
fn print_reboot_resume_disclosure() {
    println!(
        "\n{}",
        reboot_resume_arming_disclosure(REBOOT_RESUME_LIFETIME)
    );
}

/// Builds the honest capability set this agent advertises, the reboot-resume
/// armer for this platform, and whether arming is actually offered.
///
/// The advertised set must match what the session runtime actually serves on
/// this build, because the runtime *enforces* the capability gates for the
/// interactive terminal and desktop (REQ-N-012): a peer that never sees the
/// capability is answered `Unsupported` and stays interoperable. So each entry
/// is gated on the backend truly being present:
///
/// - `fs.v1` — always. The [`FileStore`] backend is cross-platform (ADR-0012,
///   ST-M003-004/005). Its gate is advisory (file ops are served regardless),
///   but advertising it lets a peer negotiate honestly.
/// - `fs.v2` — always. The drive list and the removal of a folder with its
///   content are served by the same file store on every platform; the runtime
///   enforces this gate, so they answer `Unsupported` unless it is advertised
///   here (ST-M010-001).
/// - `term.v1` — Windows only. The ConPTY interactive backend exists solely on
///   Windows; off Windows the broker answers `InteractiveUnsupported`, so
///   advertising a terminal there would be a lie (ADR-0013, ST-M003-009).
/// - `desktop.v1` — only where [`Desktop::backend_available`] reports a real
///   capture-and-input backend (Windows). A peer never sees it on a platform
///   that would answer every desktop operation `Unsupported`
///   (ADR-0015, REQ-S-009, ST-M005-001).
/// - `reboot_resume.v1` — Windows only, with the real DPAPI-backed armer
///   persisting a single-use ticket (ADR-0014, ST-M006-001); elsewhere
///   reboot-resume stays off and nothing can be armed.
fn build_reboot_resume() -> (Arc<dyn RebootResumeArmer>, Vec<String>, bool) {
    use cretlink_protocol::{CAP_FS_V1, CAP_FS_V2};

    // Cross-platform base: file operations are always available, the drive
    // list and the folder removal of the file explorer among them.
    let mut capabilities = vec![CAP_FS_V1.to_string(), CAP_FS_V2.to_string()];

    // The interactive terminal is served only where a pseudo-console backend
    // exists (Windows). Gate the advertisement on the same condition the broker
    // enforces so the two never drift.
    #[cfg(windows)]
    {
        use cretlink_protocol::CAP_TERM_V1;
        capabilities.push(CAP_TERM_V1.to_string());
    }

    // The interactive desktop is served only where a capture-and-input backend
    // is present. `backend_available()` is the single source of truth.
    if cretlink_exec::Desktop::backend_available() {
        use cretlink_protocol::CAP_DESKTOP_V1;
        capabilities.push(CAP_DESKTOP_V1.to_string());
    }

    #[cfg(windows)]
    {
        use cretlink_protocol::CAP_REBOOT_RESUME_V1;
        use cretlink_support_agent::{
            DpapiSealer, TicketStore, VaultArmer, default_ticket_store_path,
        };

        let armer = VaultArmer::new(
            DpapiSealer::new(),
            TicketStore::new(default_ticket_store_path()),
        );
        capabilities.push(CAP_REBOOT_RESUME_V1.to_string());
        (Arc::new(armer), capabilities, true)
    }
    #[cfg(not(windows))]
    {
        (
            Arc::new(cretlink_session::NoRebootResume),
            capabilities,
            false,
        )
    }
}

/// Sweeps staging trees left by earlier runs and reports honestly. A clean run
/// with nothing to do stays silent; anything removed or any failure is printed,
/// and failures are the ones a later start will retry — they are never dressed
/// up as success (REQ-N-008, REQ-F-019).
async fn sweep_stale_staging() -> Option<cretlink_support_agent::recovery::StagingInUse> {
    let (report, in_use) = cretlink_support_agent::sweep_stale_staging().await;
    if let Some(text) = cretlink_support_agent::recovery::sweep_text(&report) {
        print!("{text}");
    }
    in_use
}

/// Runs the offline residual-recovery verb (ST-M006-003) with the report and
/// exit code shared with `cretlink recover` (ST-M009-010). Without `--remove`
/// it inspects and prints the CretLink-owned residue on this machine, removing
/// nothing so the customer can see it first. With `--remove` it removes the
/// known residue and prints an honest report; a recovery that could not prove
/// a clean result exits non-zero, so a script never mistakes kept residue for
/// success. It grants no control and needs no session (REQ-F-019, REQ-N-008).
async fn run_recover(remove: bool) -> ExitCode {
    let store = TicketStore::new(default_ticket_store_path());
    let run = cretlink_support_agent::run_recover(PROGRAM, remove, &store).await;
    print!("{}", run.text);
    if run.success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn report_cleanup(handle: &JoinHandle) {
    if let Some(report) = handle.cleanup_report() {
        if report.ok {
            println!("Cleanup complete: temporary session access has been removed.");
        } else {
            println!("Cleanup did NOT fully complete — review the following:");
            for note in &report.notes {
                println!("  - {note}");
            }
        }
    }
}

fn print_identity(identity: &EffectiveIdentity) {
    print!(
        "{}",
        cretlink_support_agent::recovery::identity_text(PROGRAM, identity)
    );
}

/// The resolved transport the agent uses to reach the console: LAN discovery or
/// a fixed manual entry, or an outbound relay rendezvous carrying the same
/// end-to-end session (ADR-0016 §5). The relay is a transport choice only; every
/// downstream session step — pairing, the Session Approval Code, consent,
/// attribution, revocation, cleanup — is identical (REQ-F-005, REQ-F-016..019).
enum AgentConnection {
    /// Reach the console on the LAN: browse over mDNS when `entry` is `None`, or
    /// dial the fixed connection code / `host:port` in `entry`.
    Lan {
        /// A manual connection code or `host:port`, or `None` for mDNS discovery.
        entry: Option<String>,
    },
    /// Reach the console through an internet relay: dial `relay` outbound and
    /// rendezvous on `ticket` (ADR-0016 §5).
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

/// Parses the agent's command line into one [`AgentConnection`], failing closed
/// on any contradictory or malformed request before dialing (REQ-N-015).
///
/// A relay address (from `--relay` or `CRETLINK_RELAY`) selects the relay path;
/// otherwise the agent reaches the console on the LAN. In relay mode the LAN
/// selectors (a positional manual entry, `--discover`) are rejected and a
/// rendezvous ticket is required; an empty ticket or token is treated as absent.
/// In LAN mode the relay-only options (`--ticket`, `--token`) are rejected.
/// Environment fallbacks are consulted only once the relay path is selected, so
/// a stray `CRETLINK_RELAY_TICKET` never breaks a plain LAN run.
fn parse_agent_connection(args: impl Iterator<Item = String>) -> Result<AgentConnection, String> {
    let mut relay = None;
    let mut ticket = None;
    let mut token = None;
    let mut relay_tls = None;
    let mut relay_ca = None;
    let mut entry = None;
    let mut discover = false;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--relay" => relay = Some(args.next().ok_or("--relay needs a value")?),
            "--ticket" => ticket = Some(args.next().ok_or("--ticket needs a value")?),
            "--token" => token = Some(args.next().ok_or("--token needs a value")?),
            "--relay-tls" => relay_tls = Some(args.next().ok_or("--relay-tls needs a value")?),
            "--relay-ca" => relay_ca = Some(args.next().ok_or("--relay-ca needs a value")?),
            "--discover" => discover = true,
            "--help" | "-h" => {
                return Err(
                    "usage: cretlink-support [<connection-code|host:port> | --discover]\n       \
                     cretlink-support --relay <host:port> --ticket <hex> [--token <secret>] \
                     [--relay-tls <server-name> [--relay-ca <ca.pem>]]"
                        .to_string(),
                );
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}"));
            }
            other => entry = Some(other.to_string()),
        }
    }

    let relay_arg = relay.or_else(|| non_empty_env(ENV_RELAY));
    match relay_arg {
        Some(relay_text) => {
            if entry.is_some() {
                return Err("a manual console address cannot be combined with --relay".to_string());
            }
            if discover {
                return Err(
                    "--discover is a LAN option and cannot be combined with --relay".to_string(),
                );
            }
            let relay = relay_text
                .trim()
                .parse::<SocketAddr>()
                .map_err(|_| format!("invalid relay address (expected host:port): {relay_text}"))?;
            let ticket_text = ticket.or_else(|| non_empty_env(ENV_RELAY_TICKET)).ok_or(
                "a rendezvous ticket is required in relay mode (--ticket or CRETLINK_RELAY_TICKET)",
            )?;
            let ticket = RendezvousTicket::parse_hex(ticket_text.trim())
                .map_err(|error| format!("invalid rendezvous ticket: {error}"))?;
            let token = token
                .or_else(|| non_empty_env(ENV_RELAY_TOKEN))
                .map(|text| AdmissionToken::from_text(text.trim()));
            let tls = resolve_relay_tls(relay_tls, relay_ca)?;
            Ok(AgentConnection::Relay {
                relay,
                ticket,
                token,
                tls,
            })
        }
        None => {
            if ticket.is_some() {
                return Err("--ticket is only valid together with --relay".to_string());
            }
            if token.is_some() {
                return Err("--token is only valid together with --relay".to_string());
            }
            if relay_tls.is_some() {
                return Err("--relay-tls is only valid together with --relay".to_string());
            }
            if relay_ca.is_some() {
                return Err("--relay-ca is only valid together with --relay".to_string());
            }
            Ok(AgentConnection::Lan { entry })
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
fn resolve_relay_tls(
    relay_tls: Option<String>,
    relay_ca: Option<String>,
) -> Result<Option<RelayTls>, String> {
    let server_name = relay_tls.or_else(|| non_empty_env(ENV_RELAY_TLS));
    let ca_path = relay_ca.or_else(|| non_empty_env(ENV_RELAY_CA));
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

/// Reaches the console over whichever transport was resolved, retrying transport
/// faults and returning `Ok(None)` if the customer presses Ctrl+C first.
async fn connect_with_retries(
    connection: AgentConnection,
    identity: &TargetIdentity,
    capabilities: Vec<String>,
    armer: Arc<dyn RebootResumeArmer>,
) -> Result<Option<JoinHandle>, String> {
    match connection {
        AgentConnection::Lan { entry } => {
            connect_lan_with_retries(entry, identity, capabilities, armer).await
        }
        AgentConnection::Relay {
            relay,
            ticket,
            token,
            tls,
        } => {
            connect_relay_with_retries(relay, ticket, token, tls, identity, capabilities, armer)
                .await
        }
    }
}

/// Keeps trying to reach a console on the LAN until one is connected, or the
/// customer presses Ctrl+C (returns `Ok(None)`). A fixed manual entry (connection
/// code or `host:port`) is dialed repeatedly so the agent can be started before
/// the console; otherwise the agent keeps browsing the LAN over mDNS until a
/// console appears. Neither discovery nor a manual address grants any
/// authority — trust comes only from the Session Approval Code comparison after
/// pairing (REQ-S-006). A pairing failure (as opposed to "not reachable yet")
/// is fatal and returned, not retried.
async fn connect_lan_with_retries(
    entry: Option<String>,
    identity: &TargetIdentity,
    capabilities: Vec<String>,
    armer: Arc<dyn RebootResumeArmer>,
) -> Result<Option<JoinHandle>, String> {
    let fixed_target = match entry.as_deref() {
        None | Some("--discover") => None,
        Some(entry) => {
            let target = parse_manual_entry(entry).map_err(|error| {
                format!("could not read '{entry}' as a connection code or address: {error}")
            })?;
            Some(target.addr())
        }
    };

    match fixed_target {
        Some(addr) => {
            println!("Waiting for the technician's console at {addr} ... (press Ctrl+C to cancel)")
        }
        None => println!(
            "Searching the local network for the technician's console ... (press Ctrl+C to cancel)\n  \
             Tip: you can also pass the console's address or connection code as an argument."
        ),
    }

    loop {
        let target = match fixed_target {
            Some(addr) => Some(addr),
            None => tokio::task::spawn_blocking(discover_once)
                .await
                .unwrap_or(None),
        };

        if let Some(target) = target {
            // The live reboot probe is injected so `SystemInfo` answers are
            // probed per request against the real registry signals
            // (ST-M003-003); the identity itself is the honest one built at
            // startup (REQ-F-016, REQ-S-011). The reboot-resume armer is injected
            // here too, but arming stays gated on a live consented session and an
            // attended local gesture (ADR-0014).
            match SessionJoiner::join_with_reboot_resume(
                target,
                identity.clone(),
                PROTOCOL_V1,
                capabilities.clone(),
                Arc::new(NoAudit),
                Arc::new(sysinfo::reboot_pending),
                Arc::clone(&armer),
            )
            .await
            {
                Ok(handle) => {
                    println!("Connected to the console at {target}.");
                    return Ok(Some(handle));
                }
                // Not reachable yet (console not started, or still coming up):
                // keep waiting rather than giving up.
                Err(SessionError::Io(_)) => {
                    println!("  Console not reachable yet — retrying...");
                }
                // A reached-but-failed handshake is a real problem, not a wait.
                Err(other) => return Err(format!("could not establish the session: {other}")),
            }
        } else {
            println!("  No console found yet — still searching...");
        }

        // Wait before the next attempt, but let Ctrl+C cancel immediately.
        tokio::select! {
            _ = tokio::time::sleep(RETRY_INTERVAL) => {}
            _ = tokio::signal::ctrl_c() => return Ok(None),
        }
    }
}

/// Reaches the console through an internet relay (ADR-0016 §5): dial the relay
/// outbound, write the one-shot rendezvous preamble naming `ticket`, and let the
/// relay splice this leg onto the console's matching one. As on the LAN, a
/// transport fault — the relay unreachable, or no peer arriving before the
/// relay's rendezvous window closes — is retried; a reached-but-failed handshake
/// is fatal. The end-to-end Noise pairing, the Session Approval Code, and
/// attended consent are byte-identical to the direct path (REQ-F-005).
async fn connect_relay_with_retries(
    relay: SocketAddr,
    ticket: RendezvousTicket,
    token: Option<AdmissionToken>,
    tls: Option<RelayTls>,
    identity: &TargetIdentity,
    capabilities: Vec<String>,
    armer: Arc<dyn RebootResumeArmer>,
) -> Result<Option<JoinHandle>, String> {
    println!(
        "Connecting to the technician's console through the relay at {relay} ... (press Ctrl+C to cancel)"
    );
    if token.is_some() {
        println!("  Presenting the relay admission token.");
    }

    // Build the wss trust material once, fail-closed: a malformed pinned CA or an
    // empty OS trust store aborts here rather than being retried forever. `None`
    // keeps the raw-TCP interim (ST-M007-009, ADR-0016 §3). The connector is an
    // `Arc`-backed handle cloned cheaply into a fresh dialer each attempt.
    let wss = match &tls {
        Some(tls) => {
            let connector = WssRelayDial::connector(tls.ca_pem.as_deref()).map_err(|error| {
                format!("could not use the relay settings: {}", with_causes(&error))
            })?;
            println!(
                "  Wrapping the relay leg in wss/TLS (validating the certificate for {}).",
                tls.server_name
            );
            Some((tls.server_name.clone(), connector))
        }
        None => None,
    };

    loop {
        // A fresh dialer per attempt: the ticket and token are zeroizing secrets
        // (`Clone`, not `Copy`) cloned into each attempt's one-shot preamble and
        // erased on drop alongside the originals.
        let attempt = match &wss {
            Some((server_name, connector)) => {
                let dialer = WssRelayDial::new(
                    relay,
                    server_name.clone(),
                    connector.clone(),
                    ticket.clone(),
                    token.clone(),
                );
                join_relay_attempt(dialer, identity, &capabilities, &armer).await
            }
            None => {
                let dialer = RelayDial::new(relay, ticket.clone(), token.clone());
                join_relay_attempt(dialer, identity, &capabilities, &armer).await
            }
        };
        match attempt {
            RelayAttempt::Connected(handle) => {
                println!("Connected to the console through the relay.");
                return Ok(Some(handle));
            }
            // The relay is unreachable: keep waiting rather than giving up.
            RelayAttempt::Retry => {
                println!("  Relay not reachable yet — retrying...");
            }
            // A reached-but-failed handshake is a real problem, not a wait.
            RelayAttempt::Fatal(message) => return Err(message),
        }

        // Wait before the next attempt, but let Ctrl+C cancel immediately.
        tokio::select! {
            _ = tokio::time::sleep(RETRY_INTERVAL) => {}
            _ = tokio::signal::ctrl_c() => return Ok(None),
        }
    }
}

/// The classification of a single relay join attempt, so the raw-TCP and wss
/// dialers share one retry loop despite their different stream types.
enum RelayAttempt {
    /// The end-to-end session was established.
    Connected(JoinHandle),
    /// The relay cannot be reached yet: wait and try again.
    Retry,
    /// A reached-but-failed handshake: fatal, do not retry.
    Fatal(String),
}

/// Runs one relay join attempt over `dialer` and classifies the result. Generic
/// over the dial so the raw-TCP [`RelayDial`] and the wss [`WssRelayDial`] reuse
/// the identical join, attribution, and reboot-resume wiring (ST-M007-009).
async fn join_relay_attempt<D: Dial>(
    dialer: D,
    identity: &TargetIdentity,
    capabilities: &[String],
    armer: &Arc<dyn RebootResumeArmer>,
) -> RelayAttempt {
    match SessionJoiner::join_with_reboot_resume_over(
        dialer,
        identity.clone(),
        PROTOCOL_V1,
        capabilities.to_vec(),
        Arc::new(NoAudit),
        Arc::new(sysinfo::reboot_pending),
        Arc::clone(armer),
    )
    .await
    {
        Ok(handle) => RelayAttempt::Connected(handle),
        // The relay is not there yet: wait and try again.
        Err(SessionError::Io(_) | SessionError::Relay(RelayFailure::Unreachable(_))) => {
            RelayAttempt::Retry
        }
        // The relay answered and refused, is not trusted, or is no relay of
        // this version: trying again changes nothing, so say why and stop.
        Err(other) => RelayAttempt::Fatal(format!(
            "could not establish the session: {}",
            with_causes(&other)
        )),
    }
}

/// An error with the causes behind it, on one line. A failed relay dial says
/// in its cause what went wrong: unreachable, not trusted, no answer, or the
/// reason of the relay's refusal (ST-M007-012).
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(inner) = cause {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        cause = inner.source();
    }
    text
}

/// One mDNS browse round. Returns the first console advertising a usable
/// address, or `None` if none answered this round (the caller retries).
fn discover_once() -> Option<SocketAddr> {
    let browser = Browser::new().ok()?;
    let consoles = browser.browse(DISCOVERY_TIMEOUT).ok()?;
    let console = consoles.into_iter().find(|c| !c.addresses.is_empty())?;
    let ip = console.addresses[0];
    println!(
        "Found console '{}' at {ip}:{}",
        console
            .display_name
            .as_deref()
            .unwrap_or(&console.instance_name),
        console.port
    );
    Some(SocketAddr::new(ip, console.port))
}

/// Builds the honest [`TargetIdentity`] this agent reports to the console
/// (REQ-F-016, REQ-S-011). `elevated` is the real token state — never claimed
/// when it is not true. The version and architecture are read live from the
/// machine (real Windows version and build, native machine architecture), not
/// compile-time literals (ST-M003-003).
fn build_target_identity(identity: &EffectiveIdentity) -> TargetIdentity {
    TargetIdentity {
        hostname: detect_hostname(),
        os_version: sysinfo::os_version(),
        arch: sysinfo::machine_arch(),
        elevated: identity.elevated,
        simulated: false,
        agent_version: cretlink_shared::RELEASE_VERSION.to_string(),
        account: identity.account(),
    }
}

fn detect_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}
