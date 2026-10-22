//! The `cretlink-cli` command, and the subcommands of the `cretlink` application
//! (ST-M009-002, ST-M009-003, ADR-0018).
//!
//! One entry point for the technician's command-line work:
//!
//! - `cretlink-cli mcp` serves the MCP bridge over stdio (ADR-0004). It holds no
//!   session authority; every tool call goes to the running console over
//!   same-user local IPC and fails closed without a consented session.
//! - `cretlink-cli mcp install | uninstall | status` registers that bridge with the
//!   technician's AI clients (REQ-F-025), see [`setup`].
//! - `cretlink-cli shell` attaches a host terminal to the consented session
//!   (ST-M008-002).
//! - `cretlink-cli version` prints the release version (ST-M006-014).
//!
//! None of the subcommands opens a session, a listener or a window. The
//! dispatch is a library so the one CretLink application (ST-M009-005) can run
//! the same subcommands before it opens its window.

pub mod process;
pub mod setup;

/// The release version this build belongs to (ST-M006-014).
pub use cretlink_shared::RELEASE_VERSION;

use std::process::ExitCode;

use setup::{Action, Client, SetupOptions};

/// Exit code for a command line this program does not understand.
const EXIT_USAGE: u8 = 2;

/// Whether this build runs in the Windows GUI subsystem, without a console
/// window of its own (ST-M009-001, ADR-0018).
pub const GUI_SUBSYSTEM: bool = cfg!(all(windows, feature = "gui-subsystem"));

/// A parsed `cretlink` command line.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Print the usage text and exit successfully.
    Help,
    /// Print the release version.
    Version,
    /// Serve the MCP bridge over stdio.
    McpServe,
    /// Print the `cretlink-cli mcp` usage text.
    McpHelp,
    /// Register, unregister or report the MCP server in AI clients.
    McpSetup(Action, SetupOptions),
    /// Run the technician terminal client with the remaining arguments.
    Shell(Vec<String>),
}

/// Parses the arguments after the program name.
///
/// # Errors
///
/// Returns a message for an unknown subcommand, option or client name.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Command::Help);
    };
    match first.as_str() {
        "help" | "-h" | "--help" => no_more(args, Command::Help),
        "version" | "-V" | "--version" => no_more(args, Command::Version),
        "shell" => Ok(Command::Shell(args.collect())),
        "mcp" => parse_mcp(args),
        other => Err(format!("unknown command '{other}'")),
    }
}

fn parse_mcp(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let Some(sub) = args.next() else {
        return Ok(Command::McpServe);
    };
    let action = match sub.as_str() {
        "install" => Action::Install,
        "uninstall" => Action::Uninstall,
        "status" => Action::Status,
        "help" | "-h" | "--help" => return no_more(args, Command::McpHelp),
        other => return Err(format!("unknown 'mcp' command '{other}'")),
    };
    let mut options = SetupOptions::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--client" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--client expects a value".to_owned())?;
                let client = Client::parse(&value)?;
                if !options.clients.contains(&client) {
                    options.clients.push(client);
                }
            }
            "--dry-run" if action != Action::Status => options.dry_run = true,
            other => return Err(format!("unknown option '{other}' for 'mcp {sub}'")),
        }
    }
    Ok(Command::McpSetup(action, options))
}

fn no_more(mut args: impl Iterator<Item = String>, command: Command) -> Result<Command, String> {
    match args.next() {
        None => Ok(command),
        Some(extra) => Err(format!("unexpected argument '{extra}'")),
    }
}

/// Runs the command line (the arguments after the program name) and returns
/// the process exit code. `program` is the name the user typed — `cretlink-cli`,
/// or `cretlink` for the application (ST-M009-005) — and names the command in
/// usage text and diagnostics.
pub async fn run(program: &str, args: impl IntoIterator<Item = String>) -> ExitCode {
    if GUI_SUBSYSTEM {
        // Write to the terminal that started us, if any. Redirected handles —
        // an MCP client's pipes — are left exactly as they are.
        let _ = process::attach_parent_console();
    }
    let command = match parse(args) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("{program}: {err}");
            eprintln!();
            eprint!("{}", usage(program));
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match command {
        Command::Help => {
            print!("{}", usage(program));
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("cretlink {}", cretlink_shared::RELEASE_VERSION);
            ExitCode::SUCCESS
        }
        Command::McpServe => cretlink_mcp::serve_stdio(&format!("{program} mcp")).await,
        Command::McpHelp => {
            print!("{}", mcp_usage(program));
            ExitCode::SUCCESS
        }
        Command::McpSetup(action, options) => {
            let exe = match std::env::current_exe() {
                Ok(path) => path,
                Err(err) => {
                    eprintln!("{program}: cannot determine the path of this program: {err}");
                    return ExitCode::FAILURE;
                }
            };
            let context = setup::Context::from_env(exe);
            let mut out = std::io::stdout();
            match setup::execute(&context, &setup::SystemRunner, action, &options, &mut out) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(err) => {
                    eprintln!("{program}: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Shell(_) if GUI_SUBSYSTEM => {
            // A terminal does not wait for a GUI-subsystem process, so an
            // interactive terminal would fight the prompt for every keystroke.
            eprintln!(
                "{program}: 'shell' needs a console of its own; run it with the console build, cretlink-cli.exe."
            );
            ExitCode::from(EXIT_USAGE)
        }
        Command::Shell(rest) => {
            cretlink_shell::cli::run(&format!("{program} shell"), rest.into_iter()).await
        }
    }
}

fn usage(program: &str) -> String {
    format!(
        "\
Usage: {program} <COMMAND>

Commands:
  mcp                 Serve the CretLink MCP server over stdio. AI clients start
                      this; it acts only on the console's consented session.
  mcp install         Register CretLink with Claude Code, Codex CLI and Gemini CLI.
  mcp uninstall       Remove that registration.
  mcp status          Show where CretLink is registered.
  shell [OPTIONS]     Open a terminal on the consented session
                      (see '{program} shell --help').
  version             Print the version.
  help                Print this help.

Run '{program} mcp help' for the registration options.
"
    )
}

fn mcp_usage(program: &str) -> String {
    format!(
        "\
Usage: {program} mcp [install | uninstall | status] [OPTIONS]

Without a command, serve the CretLink MCP server over stdio. AI clients start
it this way; diagnostics go to stderr.

Commands:
  install     Register CretLink, under the name 'cretlink', in each AI client's
              per-user configuration, pointing at this program.
  uninstall   Remove the 'cretlink' registration.
  status      Show, per client, whether it is present and where CretLink is
              registered.

Options:
  --client <claude|codex|gemini>
              Act on this client only (repeatable). Default: every client
              found on PATH.
  --dry-run   Show what install or uninstall would run, and change nothing.

The registration goes through each client's own command line ('claude mcp',
'codex mcp', 'gemini mcp'). No credential or session secret is read or written.
If you move this program, run '{program} mcp install' again.
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_arguments_prints_help() {
        assert_eq!(parse(args(&[])), Ok(Command::Help));
        assert_eq!(parse(args(&["--help"])), Ok(Command::Help));
        assert_eq!(parse(args(&["help"])), Ok(Command::Help));
    }

    #[test]
    fn version_aliases() {
        for spelling in ["version", "--version", "-V"] {
            assert_eq!(parse(args(&[spelling])), Ok(Command::Version));
        }
        assert!(parse(args(&["version", "extra"])).is_err());
    }

    #[test]
    fn bare_mcp_serves_stdio() {
        assert_eq!(parse(args(&["mcp"])), Ok(Command::McpServe));
        assert_eq!(parse(args(&["mcp", "--help"])), Ok(Command::McpHelp));
    }

    #[test]
    fn mcp_setup_options() {
        assert_eq!(
            parse(args(&["mcp", "install"])),
            Ok(Command::McpSetup(Action::Install, SetupOptions::default()))
        );
        assert_eq!(
            parse(args(&[
                "mcp",
                "install",
                "--client",
                "codex",
                "--client",
                "claude",
                "--client",
                "codex",
                "--dry-run"
            ])),
            Ok(Command::McpSetup(
                Action::Install,
                SetupOptions {
                    clients: vec![Client::Codex, Client::Claude],
                    dry_run: true,
                }
            ))
        );
        assert_eq!(
            parse(args(&["mcp", "uninstall", "--client", "gemini"])),
            Ok(Command::McpSetup(
                Action::Uninstall,
                SetupOptions {
                    clients: vec![Client::Gemini],
                    dry_run: false,
                }
            ))
        );
    }

    #[test]
    fn status_has_no_dry_run() {
        assert!(parse(args(&["mcp", "status", "--dry-run"])).is_err());
    }

    #[test]
    fn unknown_input_is_rejected() {
        assert!(parse(args(&["frobnicate"])).is_err());
        assert!(parse(args(&["mcp", "frobnicate"])).is_err());
        assert!(parse(args(&["mcp", "install", "--client"])).is_err());
        assert!(parse(args(&["mcp", "install", "--client", "copilot"])).is_err());
        assert!(parse(args(&["mcp", "install", "--force"])).is_err());
    }

    #[test]
    fn shell_passes_its_arguments_through() {
        assert_eq!(
            parse(args(&["shell", "--shell", "cmd", "--help"])),
            Ok(Command::Shell(args(&["--shell", "cmd", "--help"])))
        );
    }
}
