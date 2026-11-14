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
//!   (ST-M008-002), see [`shell`].
//! - `cretlink-cli version` prints the release version (ST-M006-014).
//! - `cretlink-cli uninstall` removes the per-user install of the technician
//!   role, with the settings of the installed copy ([`settings`],
//!   ST-M007-011) and its webview profile (ST-M009-014), after showing what
//!   it removes (ST-M009-006), see [`install`].
//! - `cretlink-cli recover [--remove]` lists, or removes, what CretLink left on
//!   a customer computer (ST-M006-003, ST-M009-010): the report and the
//!   fail-closed exit codes of the support agent's recovery verb, which it
//!   replaces in the product ([`cretlink_support_agent::recovery`]), with the
//!   webview profile folders that the application's window left added to it
//!   (ST-M009-014, [`webview_profile`]). A `CretLink-Support*` copy of the
//!   application accepts this command only.
//!
//! None of the subcommands opens a session, a listener or a window. The
//! dispatch is a library so the one CretLink application (ST-M009-005) can run
//! the same subcommands before it opens its window.

pub mod install;
pub mod process;
pub mod relay_profile;
pub mod settings;
pub mod setup;
pub mod shell;
pub mod webview_profile;

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
    /// Remove the per-user install; `yes` skips the confirmation.
    Uninstall {
        /// Remove without asking (`--yes`).
        yes: bool,
    },
    /// List (or with `remove`, remove) the CretLink residue on this computer.
    Recover {
        /// Remove what was found (`--remove`); otherwise only list it.
        remove: bool,
    },
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
        "uninstall" => parse_uninstall(args),
        "recover" => parse_recover(args),
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

fn parse_uninstall(args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            other => return Err(format!("unknown option '{other}' for 'uninstall'")),
        }
    }
    Ok(Command::Uninstall { yes })
}

fn parse_recover(args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut remove = false;
    for arg in args {
        match arg.as_str() {
            "--remove" => remove = true,
            other => return Err(format!("unknown option '{other}' for 'recover'")),
        }
    }
    Ok(Command::Recover { remove })
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
    let console = if GUI_SUBSYSTEM {
        // Write to the terminal that started us, if any. Redirected handles —
        // an MCP client's pipes — are left exactly as they are.
        process::attach_parent_console()
    } else {
        process::ConsoleAttach::Inherited
    };
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
            shell::cli::run(&format!("{program} shell"), rest.into_iter()).await
        }
        Command::Uninstall { yes } => uninstall_command(program, yes, console),
        Command::Recover { remove } => recover_command(program, remove).await,
    }
}

/// `cretlink recover [--remove]` (ST-M009-010): the support agent's offline
/// residual recovery (ST-M006-003) with its report and exit codes. Inspecting
/// always succeeds; removing succeeds only when the computer was proved clean,
/// so kept or unreadable residue is never reported as success (REQ-N-008). It
/// opens no connection and grants no control.
///
/// The webview profile folders that the application's window left are part
/// of the same report (ST-M009-014): the temporary folders of copies that no
/// longer run, and the folder that earlier versions kept. The folder of a
/// copy that runs is not a leftover and is never touched; a folder whose lock
/// cannot be told is named and left alone.
async fn recover_command(program: &str, remove: bool) -> ExitCode {
    use cretlink_support_agent::recovery;
    print!(
        "{}",
        recovery::identity_text(program, &cretlink_support_agent::current_identity())
    );
    let store = cretlink_support_agent::TicketStore::for_this_process();
    let webview = WebviewResidue(webview_profile::Places::current());
    let run = recovery::run_recover_with(program, remove, &store, Some(&webview)).await;
    print!("{}", run.text);
    if run.success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The webview profile folders that the application's window left, as the
/// recovery verb lists and removes them (ST-M009-014).
struct WebviewResidue(webview_profile::Places);

impl cretlink_support_agent::OtherResidue for WebviewResidue {
    fn scan(&self, inventory: &mut cretlink_support_agent::Inventory) {
        add_webview_inventory(inventory, &webview_profile::leftovers(&self.0));
    }

    fn recover(&self, report: &mut cretlink_support_agent::RecoveryReport) {
        let mut patience = webview_profile::Patience::a_few_seconds();
        add_webview_recovery(report, &webview_profile::recover(&self.0, &mut patience));
    }
}

/// A webview folder that was left, as a resource of the inventory.
fn webview_resource(leftover: &webview_profile::Leftover) -> cretlink_support_agent::OwnedResource {
    cretlink_support_agent::OwnedResource {
        kind: cretlink_support_agent::ResourceKind::WebviewProfile,
        description: leftover.describe(),
        // A profile belongs to a process, not to a session.
        session: None,
    }
}

/// What cannot be said about the webview folders: a temporary folder that
/// could not be listed, and each folder whose lock cannot be told. Such a
/// folder is left alone; whether a copy runs with it is not known.
fn webview_unknowns(
    unreadable: Option<&String>,
    unknown: &[webview_profile::Untold],
) -> Vec<cretlink_support_agent::UnreadableKind> {
    let kind = cretlink_support_agent::ResourceKind::WebviewProfile;
    unreadable
        .cloned()
        .into_iter()
        .chain(unknown.iter().map(|folder| {
            format!(
                "{}: {}; it was left alone",
                folder.path.display(),
                folder.reason
            )
        }))
        .map(|reason| cretlink_support_agent::UnreadableKind { kind, reason })
        .collect()
}

/// Adds the webview folders that were found to the inventory: each one as a
/// resource, what cannot be said as "could not read", and an honest "checked
/// and empty" when there is nothing of either (ST-M009-014).
fn add_webview_inventory(
    inventory: &mut cretlink_support_agent::Inventory,
    leftovers: &webview_profile::Leftovers,
) {
    inventory
        .found
        .extend(leftovers.found.iter().map(webview_resource));
    let unknowns = webview_unknowns(leftovers.unreadable.as_ref(), &leftovers.unknown);
    if leftovers.found.is_empty() && unknowns.is_empty() {
        inventory
            .asserted_empty
            .push(cretlink_support_agent::ResourceKind::WebviewProfile);
    }
    inventory.unreadable.extend(unknowns);
}

/// Adds what happened to the webview folders to the recovery report: found,
/// removed, kept with the reason and what to do by hand, and what cannot be
/// said (ST-M009-014).
fn add_webview_recovery(
    report: &mut cretlink_support_agent::RecoveryReport,
    recovery: &webview_profile::Recovery,
) {
    report
        .found
        .extend(recovery.found.iter().map(webview_resource));
    report
        .removed
        .extend(recovery.removed.iter().map(webview_resource));
    report
        .failed
        .extend(recovery.kept.iter().map(|(leftover, reason)| {
            cretlink_support_agent::RecoveryFailure {
                kind: cretlink_support_agent::ResourceKind::WebviewProfile,
                description: leftover.describe(),
                reason: reason.clone(),
                remediation: format!(
                    "close every CretLink window, then delete the folder manually: {}",
                    leftover.path.display()
                ),
            }
        }));
    report.unreadable.extend(webview_unknowns(
        recovery.unreadable.as_ref(),
        &recovery.unknown,
    ));
}

/// `cretlink uninstall`: shows what it will remove, asks, then removes the
/// per-user install (ST-M009-006). `console` tells whether a terminal can be
/// asked: a GUI-subsystem build without usable standard handles — started by
/// the Windows uninstall entry, or from a terminal that does not wait for it —
/// asks in a message box instead.
fn uninstall_command(program: &str, yes: bool, console: process::ConsoleAttach) -> ExitCode {
    let in_box = GUI_SUBSYSTEM && console != process::ConsoleAttach::Inherited;
    let tell = |title: &str, text: &str, failed: bool| {
        if in_box {
            if failed {
                process::alert(title, text);
            } else {
                process::inform(title, text);
            }
        } else if failed {
            eprintln!("{program}: {}", text.trim_end());
        } else {
            println!("{}", text.trim_end());
        }
    };
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            tell(
                "CretLink",
                &format!("cannot determine the path of this program: {err}"),
                true,
            );
            return ExitCode::FAILURE;
        }
    };
    let Some(layout) = install::Layout::current() else {
        tell(
            "CretLink",
            "cannot find this user's folders (HOME, LOCALAPPDATA or APPDATA is not set)",
            true,
        );
        return ExitCode::FAILURE;
    };
    let context = setup::Context::from_env(exe.clone());
    let system = install::OsSystem;
    let plan = match install::uninstall_plan(&layout, &system, &context, &exe) {
        Ok(plan) => plan,
        Err(install::InstallError::NotInstalled(location)) => {
            tell(
                "CretLink",
                &format!(
                    "CretLink is not installed for this user (nothing of it in {}); nothing to do.",
                    location.display()
                ),
                false,
            );
            return ExitCode::SUCCESS;
        }
        Err(err @ install::InstallError::CustomerCopy) => {
            tell("CretLink", &err.to_string(), true);
            return ExitCode::from(EXIT_USAGE);
        }
        Err(err) => {
            tell("CretLink", &err.to_string(), true);
            return ExitCode::FAILURE;
        }
    };
    let text = uninstall_plan_text(&plan);
    let confirmed = if yes {
        if !in_box {
            print!("{text}");
        }
        true
    } else if in_box {
        process::confirm("Uninstall CretLink", &format!("{text}\nUninstall now?"))
    } else {
        print!("{text}Uninstall now? [y/N] ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        confirmed(&mut std::io::stdin().lock())
    };
    if !confirmed {
        tell(
            "CretLink",
            "Uninstall cancelled; nothing was changed.",
            false,
        );
        return ExitCode::FAILURE;
    }
    match install::uninstall(
        &layout,
        &system,
        &setup::SystemRunner,
        &context,
        &plan,
        &exe,
    ) {
        Ok(report) => {
            let complete = report.complete();
            tell(
                "Uninstall CretLink",
                &uninstall_report_text(&report, &plan.audit_dir),
                !complete,
            );
            if complete {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            tell(
                "Uninstall CretLink",
                &format!("{err}. Nothing was changed."),
                true,
            );
            ExitCode::FAILURE
        }
    }
}

/// Reads one answer line: only `y` or `yes` (any case) confirms; end of
/// input or anything else does not.
fn confirmed(input: &mut dyn std::io::BufRead) -> bool {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

/// What uninstall will remove and what stays, before anything changes.
fn uninstall_plan_text(plan: &install::UninstallPlan) -> String {
    let mut text = format!(
        "Uninstall CretLink from {}.\nThis removes:\n",
        plan.location.display()
    );
    for line in plan.lines() {
        text.push_str(&format!("  - {line}\n"));
    }
    text.push_str("This stays:\n");
    for line in &plan.kept {
        text.push_str(&format!("  - {line}\n"));
    }
    text.push_str(&format!(
        "  - The audit trail in {}\n",
        plan.audit_dir.display()
    ));
    text
}

/// What uninstall did, failures first.
fn uninstall_report_text(report: &install::UninstallReport, audit_dir: &std::path::Path) -> String {
    let mut text = String::new();
    let mut section = |title: &str, lines: &[String]| {
        if lines.is_empty() {
            return;
        }
        text.push_str(title);
        text.push('\n');
        for line in lines {
            text.push_str(&format!("  - {line}\n"));
        }
    };
    section("Could not remove:", &report.failed);
    section("Removed:", &report.removed);
    section("Removed once CretLink has closed:", &report.pending);
    let mut kept = report.kept.clone();
    kept.push(format!("The audit trail in {}", audit_dir.display()));
    section("Left in place:", &kept);
    if report.complete() {
        text.push_str("CretLink was uninstalled.\n");
    } else {
        text.push_str("CretLink was not completely uninstalled; remove what is listed under 'Could not remove' by hand, or run 'cretlink uninstall' again.\n");
    }
    text
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
  uninstall [--yes]   Remove CretLink installed for this user: its files, its
                      settings (the relay and its key), menu entry, uninstall
                      entry, webview profile and the AI client registrations
                      it made. Shows what it removes and what stays, and asks
                      first, unless --yes. The audit trail stays, and so does
                      a relay profile cretlink.toml next to the program:
                      uninstall says where it lies and whether it holds a key.
  recover [--remove]  List what CretLink left on this computer (staged files,
                      reboot-resume tickets, webview profile folders); with
                      --remove, remove it. Exits with an error when something
                      could not be removed.
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
    fn uninstall_asks_unless_told_yes() {
        assert_eq!(
            parse(args(&["uninstall"])),
            Ok(Command::Uninstall { yes: false })
        );
        for spelling in ["--yes", "-y"] {
            assert_eq!(
                parse(args(&["uninstall", spelling])),
                Ok(Command::Uninstall { yes: true })
            );
        }
        assert!(parse(args(&["uninstall", "--force"])).is_err());
        // Install is a choice of the application's start screen, not a
        // subcommand.
        assert!(parse(args(&["install"])).is_err());
    }

    #[test]
    fn only_yes_confirms_an_uninstall() {
        for (answer, expected) in [
            ("y\n", true),
            ("YES\n", true),
            (" yes \r\n", true),
            ("\n", false),
            ("no\n", false),
            ("yep\n", false),
            ("", false),
        ] {
            assert_eq!(confirmed(&mut answer.as_bytes()), expected, "{answer:?}");
        }
    }

    #[test]
    fn the_uninstall_plan_says_what_stays() {
        let plan = install::UninstallPlan {
            location: std::path::PathBuf::from("/home/ann/.local/bin"),
            files: vec![std::path::PathBuf::from("/home/ann/.local/bin/cretlink")],
            settings: vec![std::path::PathBuf::from(
                "/home/ann/.local/share/CretLink/settings.toml",
            )],
            after_exit: false,
            entry: None,
            uninstall_entry: false,
            registrations: vec![],
            kept: vec![
                "The Codex CLI registration, which now starts /x mcp".to_owned(),
                "The relay profile /home/ann/.local/bin/cretlink.toml, which CretLink did not \
                 write and does not remove. It holds the admission key of a relay in readable \
                 form: remove the file yourself when nobody else should be able to read that key"
                    .to_owned(),
            ],
            webview: Some(std::path::PathBuf::from(
                "/home/ann/.local/share/CretLink/webview",
            )),
            webview_after_close: false,
            legacy_webview: Some(std::path::PathBuf::from(
                "/home/ann/.local/share/dev.cretlink.app",
            )),
            audit_dir: std::path::PathBuf::from("/home/ann/.local/share/CretLink/audit"),
        };
        let text = uninstall_plan_text(&plan);
        // The webview profiles go (ST-M009-014); the audit trail, which lies
        // next to the installed copy's profile, stays.
        assert!(
            text.contains(
                "  - The webview profile /home/ann/.local/share/CretLink/webview\n  - The \
                 webview profile of earlier versions /home/ann/.local/share/dev.cretlink.app\n"
            ),
            "{text}"
        );
        // From the installed copy's own window, which holds the profile.
        let from_window = uninstall_plan_text(&plan.clone().from_its_window());
        assert!(
            from_window.contains(
                "  - The webview profile /home/ann/.local/share/CretLink/webview (removed once \
                 CretLink has closed)\n"
            ),
            "{from_window}"
        );
        // A relay profile next to the program stays, and the plan says so
        // under what stays, with where it lies and that it holds a key.
        let stays = text.split_once("This stays:\n").map(|(_, stays)| stays);
        assert!(
            stays.is_some_and(|stays| stays.contains(
                "  - The relay profile /home/ann/.local/bin/cretlink.toml, which CretLink did \
                 not write and does not remove. It holds the admission key of a relay in \
                 readable form"
            )),
            "{text}"
        );
        assert!(
            text.contains("This removes:\n  - /home/ann/.local/bin/cretlink\n"),
            "{text}"
        );
        // The settings hold the relay's key: the plan says that they go too.
        assert!(
            text.contains(
                "  - Your settings (the relay with its key, the network to wait on): \
                 /home/ann/.local/share/CretLink/settings.toml\n"
            ),
            "{text}"
        );
        assert!(text.contains("  - The Codex CLI registration, which now starts /x mcp\n"));
        assert!(text.contains("  - The audit trail in /home/ann/.local/share/CretLink/audit\n"));

        let report = install::UninstallReport {
            removed: vec!["/home/ann/.local/bin/cretlink".to_owned()],
            failed: vec!["/home/ann/.local/bin/cretlink-cli: denied".to_owned()],
            ..install::UninstallReport::default()
        };
        let text = uninstall_report_text(&report, &plan.audit_dir);
        assert!(
            text.starts_with("Could not remove:\n  - /home/ann/.local/bin/cretlink-cli: denied\n"),
            "{text}"
        );
        assert!(text.contains("The audit trail in /home/ann/.local/share/CretLink/audit"));
        assert!(text.contains("not completely uninstalled"));
        // What stayed is said again once the uninstall is done.
        let report = install::UninstallReport {
            kept: plan.kept.clone(),
            ..install::UninstallReport::default()
        };
        let text = uninstall_report_text(&report, &plan.audit_dir);
        let left = text.split_once("Left in place:\n").map(|(_, left)| left);
        assert!(
            left.is_some_and(|left| left
                .contains("  - The relay profile /home/ann/.local/bin/cretlink.toml, which")),
            "{text}"
        );
    }

    fn leftover(path: &str, origin: webview_profile::Origin) -> webview_profile::Leftover {
        webview_profile::Leftover {
            path: std::path::PathBuf::from(path),
            origin,
        }
    }

    fn untold(path: &str) -> webview_profile::Untold {
        webview_profile::Untold {
            path: std::path::PathBuf::from(path),
            reason: "its lock could not be opened (permission denied)".to_owned(),
        }
    }

    #[test]
    fn recover_lists_the_webview_folders_with_the_other_residue() {
        use cretlink_support_agent::recovery::inventory_text;
        use cretlink_support_agent::{Inventory, ResourceKind};
        use webview_profile::{Leftovers, Origin};

        // Nothing left: an honest "checked and empty", and a clean computer.
        let mut clean = Inventory::default();
        add_webview_inventory(&mut clean, &Leftovers::default());
        assert_eq!(clean.asserted_empty, vec![ResourceKind::WebviewProfile]);
        assert!(clean.is_clean());
        let text = inventory_text("cretlink", &clean);
        assert!(
            text.contains("Checked and empty: webview profile."),
            "{text}"
        );
        assert!(text.contains("This machine is clean"), "{text}");

        // The folder of a copy that was killed and the one of an earlier
        // version: found, with the removal command, and not asserted empty.
        let mut inventory = Inventory::default();
        add_webview_inventory(
            &mut inventory,
            &Leftovers {
                found: vec![
                    leftover("/tmp/CretLink-webview-01", Origin::Stale),
                    leftover(
                        "/home/ann/.local/share/dev.cretlink.app",
                        Origin::EarlierVersion,
                    ),
                ],
                ..Leftovers::default()
            },
        );
        assert!(inventory.asserted_empty.is_empty());
        assert!(
            inventory
                .found
                .iter()
                .all(|resource| resource.kind == ResourceKind::WebviewProfile
                    && resource.session.is_none())
        );
        let text = inventory_text("cretlink", &inventory);
        assert!(text.contains("Found 2 resource(s)"), "{text}");
        assert!(
            text.contains(
                "  - webview profile: /tmp/CretLink-webview-01 (left by a copy that no longer \
                 runs)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "  - webview profile: /home/ann/.local/share/dev.cretlink.app (left by an \
                 earlier version)\n"
            ),
            "{text}"
        );
        assert!(text.contains("run:  cretlink recover --remove"), "{text}");

        // A folder whose lock cannot be told is named, and the computer is
        // not called clean.
        let mut unknown = Inventory::default();
        add_webview_inventory(
            &mut unknown,
            &Leftovers {
                unknown: vec![untold("/tmp/CretLink-webview-02")],
                ..Leftovers::default()
            },
        );
        assert!(unknown.asserted_empty.is_empty());
        assert!(!unknown.is_clean());
        let text = inventory_text("cretlink", &unknown);
        assert!(
            text.contains(
                "! Could not read webview profile (/tmp/CretLink-webview-02: its lock could not \
                 be opened (permission denied); it was left alone)"
            ),
            "{text}"
        );
        assert!(!text.contains("is clean"), "{text}");

        // A temporary folder that cannot be listed is unknown, never clean.
        let mut unlisted = Inventory::default();
        add_webview_inventory(
            &mut unlisted,
            &Leftovers {
                unreadable: Some("the temporary folder /tmp could not be listed (denied)".into()),
                ..Leftovers::default()
            },
        );
        assert!(unlisted.asserted_empty.is_empty());
        assert!(!unlisted.is_clean());
        assert!(inventory_text("cretlink", &unlisted).contains(
            "! Could not read webview profile (the temporary folder /tmp could not be \
                 listed (denied))"
        ));
    }

    #[test]
    fn recover_reports_a_webview_folder_it_could_not_remove_and_fails() {
        use cretlink_support_agent::recovery::recovery_report_text;
        use cretlink_support_agent::{RecoveryReport, ResourceKind};
        use webview_profile::{Origin, Recovery};

        let removed = leftover("/tmp/CretLink-webview-01", Origin::Stale);
        let kept = leftover("/tmp/CretLink-webview-02", Origin::Stale);
        let mut report = RecoveryReport::default();
        add_webview_recovery(
            &mut report,
            &Recovery {
                found: vec![removed.clone(), kept.clone()],
                removed: vec![removed],
                kept: vec![(kept, "a file in it is in use".to_owned())],
                ..Recovery::default()
            },
        );
        assert!(!report.ok(), "a kept folder is never a success");
        assert_eq!(report.failed[0].kind, ResourceKind::WebviewProfile);
        let text = recovery_report_text(&report);
        assert!(text.contains("Removed 1 of 2 resource(s) found."), "{text}");
        assert!(
            text.contains("  - removed webview profile: /tmp/CretLink-webview-01"),
            "{text}"
        );
        assert!(
            text.contains("KEPT 1 resource(s) that could NOT be removed"),
            "{text}"
        );
        assert!(
            text.contains("      reason:      a file in it is in use\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "      remediation: close every CretLink window, then delete the folder \
                 manually: /tmp/CretLink-webview-02\n"
            ),
            "{text}"
        );
        assert!(text.contains("Recovery did NOT fully complete"), "{text}");

        // Everything removed: clean. Nothing there: nothing to remove.
        let gone = leftover("/tmp/CretLink-webview-03", Origin::Stale);
        let mut report = RecoveryReport::default();
        add_webview_recovery(
            &mut report,
            &Recovery {
                found: vec![gone.clone()],
                removed: vec![gone],
                ..Recovery::default()
            },
        );
        assert!(report.ok());
        assert!(recovery_report_text(&report).contains("this machine is clean"));
        let mut report = RecoveryReport::default();
        add_webview_recovery(&mut report, &Recovery::default());
        assert!(recovery_report_text(&report).contains("nothing to remove"));

        // A folder whose lock cannot be told was left alone: that is said,
        // and the recovery does not call the computer clean.
        let mut report = RecoveryReport::default();
        add_webview_recovery(
            &mut report,
            &Recovery {
                unknown: vec![untold("/tmp/CretLink-webview-04")],
                ..Recovery::default()
            },
        );
        assert!(!report.ok());
        let text = recovery_report_text(&report);
        assert!(!text.contains("nothing to remove"), "{text}");
        assert!(
            text.contains(
                "! Could not read webview profile (/tmp/CretLink-webview-04: its lock could not \
                 be opened (permission denied); it was left alone) — state unknown."
            ),
            "{text}"
        );
        assert!(text.contains("Recovery did NOT fully complete"), "{text}");
    }

    #[test]
    fn recover_lists_unless_told_to_remove() {
        assert_eq!(
            parse(args(&["recover"])),
            Ok(Command::Recover { remove: false })
        );
        assert_eq!(
            parse(args(&["recover", "--remove"])),
            Ok(Command::Recover { remove: true })
        );
        assert!(parse(args(&["recover", "--force"])).is_err());
        assert!(parse(args(&["recover", "remove"])).is_err());
    }

    #[test]
    fn shell_passes_its_arguments_through() {
        assert_eq!(
            parse(args(&["shell", "--shell", "cmd", "--help"])),
            Ok(Command::Shell(args(&["--shell", "cmd", "--help"])))
        );
    }
}
