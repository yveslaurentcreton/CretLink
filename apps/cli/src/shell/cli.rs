//! The host-terminal client behind `cretlink-cli shell` and `cretlink shell`
//! (ST-M008-002, ST-M009-002, ADR-0004, ADR-0013).
//!
//! A technician runs it inside a host terminal (Windows Terminal, WezTerm, …)
//! to work an interactive shell on the CretLink console's one explicitly
//! selected, consented session — an SSH-like experience with no SSH, no key, no
//! listener, and nothing installed on the customer computer (REQ-F-023,
//! REQ-S-015). It holds no session authority: it connects over the same
//! kernel-enforced same-user local IPC as the MCP bridge and drives the existing
//! versioned interactive-terminal wire ops (see [`super::engine`]). With no
//! consented session, or after a revoke or stop, it fails closed and nothing
//! runs (REQ-S-009).
//!
//! # stdout is sacred
//!
//! During a session stdout carries the remote terminal's raw VT output only.
//! Every diagnostic — the identity banner, connection errors, the end-of-run
//! summary, and any panic — goes to **stderr**, so nothing this client emits can
//! corrupt the terminal stream. (`--help` prints usage to stdout before any
//! session exists.)

use std::collections::BTreeMap;
use std::process::ExitCode;

use super::engine::{self, Attached, InputEvent, OpenConfig, RunEnd, SessionError};
use super::tty;
use cretlink_mcp::IpcEndpoint;
use cretlink_protocol::Shell;
use tokio::sync::mpsc;

/// Bounded depth of the local-input queue to the engine. Keystrokes are tiny and
/// the remote pty applies its own backpressure, so a small buffer is ample.
const INPUT_CHANNEL_CAPACITY: usize = 256;

/// Fallback terminal size when neither an argument nor the host console supplies
/// one.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

// Exit codes. Remote process codes pass through directly (a remote `exit 1` is a
// local `1`); the codes below are the client's own outcomes, kept clear of the
// common small remote codes so a technician can tell them apart.
const EXIT_USAGE: u8 = 2;
const EXIT_NO_CONSENT: u8 = 3;
const EXIT_INCOMPATIBLE: u8 = 4;
const EXIT_REFUSED: u8 = 5;
const EXIT_LINK: u8 = 6;
const EXIT_REVOKED: u8 = 7;
const EXIT_DROPPED: u8 = 8;
const EXIT_REMOTE_UNKNOWN: u8 = 9;
/// A remote exit code that does not fit a `u8` is reported as this sentinel; the
/// honest code is still printed in the end-of-run summary.
const EXIT_REMOTE_UNREPRESENTABLE: u8 = 10;

/// Runs the terminal client with `argv` (the arguments after the command name)
/// and returns the process exit code. `program` names the command in usage and
/// prefixes every stderr diagnostic (`cretlink-cli shell` or `cretlink shell`).
///
/// The caller installs a panic hook that writes to stderr only.
pub async fn run(program: &str, argv: impl Iterator<Item = String>) -> ExitCode {
    let args = match parse_args(argv) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("{program}: {err}");
            eprintln!();
            eprint!("{}", usage(program));
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if args.help {
        print!("{}", usage(program));
        return ExitCode::SUCCESS;
    }

    let endpoint = select_endpoint(args.endpoint.as_deref());
    let mut stream = match cretlink_mcp::ipc::connect(&endpoint).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!(
                "{program}: could not reach the CretLink console over local IPC endpoint '{}': {err}",
                endpoint.name()
            );
            eprintln!("{program}: is a CretLink console running for this user? Nothing ran.");
            return ExitCode::from(EXIT_LINK);
        }
    };

    let (cols, rows) = initial_size(&args);
    let config = OpenConfig {
        shell: args.shell,
        cols,
        rows,
        working_dir: args.working_dir,
        env: args.env,
    };

    // Open and attach first, so a fail-closed idle console is reported without
    // ever touching the host console mode.
    let attached = match engine::open_and_attach(&mut stream, &config).await {
        Ok(attached) => attached,
        Err(err) => {
            report_session_error(program, &endpoint, &err);
            return ExitCode::from(session_error_code(&err));
        }
    };
    print_banner(program, &attached);

    // Only now put the host console into raw mode; best-effort so a non-console
    // stdin/stdout (a pipe, a redirected file) still works line-buffered.
    let raw = match tty::RawMode::enable() {
        Ok(guard) => Some(guard),
        Err(err) => {
            eprintln!(
                "{program}: continuing without local raw mode ({err}); keystrokes may be line-buffered."
            );
            None
        }
    };

    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(INPUT_CHANNEL_CAPACITY);
    let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let output_task = tokio::spawn(tty::output_to_stdout(output_rx));
    let input_task = tokio::spawn(tty::stdin_to_input(input_tx));

    let end = engine::pump(stream, attached.term, input_rx, output_tx).await;

    // The run is over: stop reading local input and drain any final output.
    input_task.abort();
    let _ = output_task.await;
    // Restore the host console before the summary so the technician's prompt
    // returns to normal. Outside Windows the guard is a no-op without `Drop`.
    #[cfg_attr(not(windows), allow(clippy::drop_non_drop))]
    drop(raw);

    let (code, summary) = describe_end(&end);
    eprintln!("{program}: {summary}");
    ExitCode::from(code)
}

/// Parsed command-line arguments.
#[derive(Debug)]
struct Args {
    endpoint: Option<String>,
    shell: Shell,
    cols: Option<u16>,
    rows: Option<u16>,
    working_dir: Option<String>,
    env: BTreeMap<String, String>,
    help: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            endpoint: None,
            shell: Shell::PowerShell,
            cols: None,
            rows: None,
            working_dir: None,
            env: BTreeMap::new(),
            help: false,
        }
    }
}

fn usage(program: &str) -> String {
    format!(
        "\
Usage: {program} [OPTIONS]

Attach a host terminal to the CretLink console's selected, consented session.
The console is the sole routing and consent authority; with no consented session
this exits without running anything.

Options:
  --endpoint <NAME>     Same-user IPC endpoint name (default: the user's console,
                        or the CRETLINK_IPC_ENDPOINT environment variable).
  --shell <powershell|cmd>
                        Remote shell to run (default: powershell).
  --cols <N>            Initial terminal width  (default: host size or 80).
  --rows <N>            Initial terminal height (default: host size or 24).
  --working-dir <PATH>  Remote working directory for the shell.
  --env <KEY=VALUE>     Extra remote environment override (repeatable).
  -h, --help            Print this help and exit.

While attached, press Ctrl-] to detach (leaving the remote terminal running).
Ctrl-C is delivered to the remote shell, not this client.
"
    )
}

/// Parses the argument list (already skipping the program name).
fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args::default();
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-h" | "--help" => args.help = true,
            "--endpoint" => args.endpoint = Some(value_for(&mut argv, "--endpoint")?),
            "--shell" => args.shell = parse_shell(&value_for(&mut argv, "--shell")?)?,
            "--cols" => args.cols = Some(parse_u16(&value_for(&mut argv, "--cols")?, "--cols")?),
            "--rows" => args.rows = Some(parse_u16(&value_for(&mut argv, "--rows")?, "--rows")?),
            "--working-dir" | "--cwd" => {
                args.working_dir = Some(value_for(&mut argv, "--working-dir")?);
            }
            "--env" => {
                let entry = value_for(&mut argv, "--env")?;
                let (key, val) = entry
                    .split_once('=')
                    .ok_or_else(|| format!("--env expects KEY=VALUE, got '{entry}'"))?;
                if key.is_empty() {
                    return Err(format!("--env key must not be empty in '{entry}'"));
                }
                args.env.insert(key.to_owned(), val.to_owned());
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(args)
}

fn value_for(argv: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    argv.next().ok_or_else(|| format!("{flag} expects a value"))
}

fn parse_shell(value: &str) -> Result<Shell, String> {
    match value.to_ascii_lowercase().as_str() {
        "powershell" | "pwsh" | "ps" => Ok(Shell::PowerShell),
        "cmd" | "cmd.exe" => Ok(Shell::Cmd),
        other => Err(format!(
            "unknown shell '{other}' (expected 'powershell' or 'cmd')"
        )),
    }
}

fn parse_u16(value: &str, flag: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("{flag} expects a number 0..=65535, got '{value}'"))
}

/// Selects the IPC endpoint: an explicit `--endpoint`, else a non-empty
/// `CRETLINK_IPC_ENDPOINT`, else the current user's default console endpoint.
/// This never changes the transport or its same-user authority — only the
/// logical endpoint name (mirrors the MCP bridge).
fn select_endpoint(arg: Option<&str>) -> IpcEndpoint {
    if let Some(name) = arg.map(str::trim).filter(|name| !name.is_empty()) {
        return IpcEndpoint::named(name.to_owned());
    }
    match std::env::var("CRETLINK_IPC_ENDPOINT") {
        Ok(name) if !name.trim().is_empty() => IpcEndpoint::named(name),
        _ => IpcEndpoint::user_default(),
    }
}

/// The initial terminal size: an explicit argument wins per-dimension, then the
/// host console size, then the fixed fallback.
fn initial_size(args: &Args) -> (u16, u16) {
    let host = tty::terminal_size();
    let cols = args
        .cols
        .or(host.map(|(cols, _)| cols))
        .unwrap_or(DEFAULT_COLS);
    let rows = args
        .rows
        .or(host.map(|(_, rows)| rows))
        .unwrap_or(DEFAULT_ROWS);
    (cols, rows)
}

/// Prints the routed identity banner to stderr so the technician sees exactly
/// which consented session and machine the terminal is bound to (REQ-F-016).
fn print_banner(program: &str, attached: &Attached) {
    let target = &attached.target;
    let mut tags = String::new();
    if target.elevated {
        tags.push_str(", elevated");
    }
    if target.simulated {
        tags.push_str(", SIMULATED");
    }
    eprintln!(
        "{program}: attached to an interactive terminal on {} ({}, {}{}).",
        target.hostname, target.os_version, target.arch, tags
    );
    eprintln!(
        "{program}: session {}, console IPC v{}.",
        attached.session, attached.console_protocol
    );
    eprintln!(
        "{program}: press Ctrl-] to detach (the remote terminal keeps running); Ctrl-C goes to the remote shell."
    );
}

/// Reports a fail-closed open/attach failure honestly to stderr.
fn report_session_error(program: &str, endpoint: &IpcEndpoint, err: &SessionError) {
    match err {
        SessionError::NoConsentedSession => {
            eprintln!(
                "{program}: the CretLink console has no consented active session; nothing ran (fail-closed)."
            );
            eprintln!(
                "{program}: select a target and grant attended consent in the console, then retry."
            );
        }
        SessionError::IncompatibleVersion { console, client } => {
            eprintln!(
                "{program}: incompatible IPC contract: console {console}, client {client}. Nothing ran."
            );
        }
        SessionError::UnsupportedTerminalMinor { console } => {
            eprintln!(
                "{program}: the console (IPC v{console}) is too old to serve an interactive terminal. Nothing ran."
            );
        }
        SessionError::Refused(refused) => {
            eprintln!("{program}: the console refused the terminal: {refused}. Nothing ran.");
        }
        SessionError::Link => {
            eprintln!(
                "{program}: lost the local IPC link to endpoint '{}' during setup. Nothing ran.",
                endpoint.name()
            );
        }
    }
}

fn session_error_code(err: &SessionError) -> u8 {
    match err {
        SessionError::NoConsentedSession => EXIT_NO_CONSENT,
        SessionError::IncompatibleVersion { .. }
        | SessionError::UnsupportedTerminalMinor { .. } => EXIT_INCOMPATIBLE,
        SessionError::Refused(_) => EXIT_REFUSED,
        SessionError::Link => EXIT_LINK,
    }
}

/// Maps a run outcome to an exit code and a one-line honest summary.
fn describe_end(end: &RunEnd) -> (u8, String) {
    match end {
        RunEnd::Exited { outcome, exit_code } => match exit_code {
            Some(code) => match u8::try_from(*code) {
                Ok(byte) => (
                    byte,
                    format!("remote terminal exited ({outcome:?}, code {code})."),
                ),
                Err(_) => (
                    EXIT_REMOTE_UNREPRESENTABLE,
                    format!("remote terminal exited ({outcome:?}, code {code})."),
                ),
            },
            None => (
                EXIT_REMOTE_UNKNOWN,
                format!("remote terminal exited ({outcome:?}, no exit code)."),
            ),
        },
        RunEnd::Revoked => (
            EXIT_REVOKED,
            "the session was revoked or stopped; the terminal was torn down (fail-closed)."
                .to_owned(),
        ),
        RunEnd::Dropped => (
            EXIT_DROPPED,
            "this consumer fell too far behind and was dropped; the remote terminal is unaffected."
                .to_owned(),
        ),
        RunEnd::Detached => (
            0,
            "detached; the remote terminal keeps running for other consumers.".to_owned(),
        ),
        RunEnd::LinkLost => (
            EXIT_LINK,
            "the local IPC link was lost before an honest end frame arrived.".to_owned(),
        ),
    }
}
