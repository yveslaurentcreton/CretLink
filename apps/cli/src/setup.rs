//! `mcp install | uninstall | status` — register the CretLink MCP
//! server with the technician's AI clients (REQ-F-025, ST-M009-003).
//!
//! Every change goes through the client's own command line (`claude mcp`,
//! `codex mcp`, `gemini mcp`) in its per-user scope, so CretLink never edits a
//! file the client itself may be writing. Existing registrations are only
//! *read* from the client's configuration file, to keep `install` idempotent
//! and to report `status`. The registration points at the absolute path of the
//! running program with the single argument `mcp`; it carries no environment,
//! credential or session secret (REQ-F-015).
//!
//! The work is split into reading the current state, deciding the steps, and
//! running them through a [`Runner`], so the decisions are testable without the
//! real clients.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;

/// The name CretLink registers under in every client.
pub const SERVER_NAME: &str = "cretlink";

/// The argument that makes the program serve MCP over stdio.
const SERVE_ARG: &str = "mcp";

/// What a setup command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Register CretLink, or update a registration that points elsewhere.
    Install,
    /// Remove the CretLink registration.
    Uninstall,
    /// Report the registration per client.
    Status,
}

/// Options shared by the setup commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupOptions {
    /// The clients to act on; empty means every supported client.
    pub clients: Vec<Client>,
    /// Show the steps without running them (install and uninstall only).
    pub dry_run: bool,
}

/// A supported MCP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    /// Claude Code (`claude`).
    Claude,
    /// Codex CLI (`codex`).
    Codex,
    /// Gemini CLI (`gemini`).
    Gemini,
}

impl Client {
    /// Every supported client, in display order.
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Gemini];

    /// Parses a `--client` value.
    ///
    /// # Errors
    ///
    /// Returns a message naming the accepted values.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" | "codex-cli" => Ok(Self::Codex),
            "gemini" | "gemini-cli" => Ok(Self::Gemini),
            other => Err(format!(
                "unknown client '{other}' (expected 'claude', 'codex' or 'gemini')"
            )),
        }
    }

    /// The product name shown to the technician.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Gemini => "Gemini CLI",
        }
    }

    /// The client's command-line program.
    fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    /// The client's per-user configuration file, where its registrations live.
    #[must_use]
    pub fn config_path(self, context: &Context) -> Option<PathBuf> {
        match self {
            Self::Claude => match &context.claude_config_dir {
                Some(dir) => Some(dir.join(".claude.json")),
                None => context.home.as_ref().map(|home| home.join(".claude.json")),
            },
            Self::Codex => match &context.codex_home {
                Some(dir) => Some(dir.join("config.toml")),
                None => context
                    .home
                    .as_ref()
                    .map(|home| home.join(".codex").join("config.toml")),
            },
            Self::Gemini => context
                .home
                .as_ref()
                .map(|home| home.join(".gemini").join("settings.json")),
        }
    }

    /// Arguments that register `exe mcp` under [`SERVER_NAME`].
    fn add_args(self, exe: &str) -> Vec<String> {
        let list: &[&str] = match self {
            Self::Claude => &[
                "mcp",
                "add",
                "--scope",
                "user",
                SERVER_NAME,
                "--",
                exe,
                SERVE_ARG,
            ],
            Self::Codex => &["mcp", "add", SERVER_NAME, "--", exe, SERVE_ARG],
            Self::Gemini => &["mcp", "add", "--scope", "user", SERVER_NAME, exe, SERVE_ARG],
        };
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// Arguments that remove the [`SERVER_NAME`] registration.
    fn remove_args(self) -> Vec<String> {
        let list: &[&str] = match self {
            Self::Claude => &["mcp", "remove", "--scope", "user", SERVER_NAME],
            Self::Codex => &["mcp", "remove", SERVER_NAME],
            Self::Gemini => &["mcp", "remove", "--scope", "user", SERVER_NAME],
        };
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// Whether `add` fails when the name is already registered, so an update
    /// must remove the old entry first. Codex replaces the entry in place.
    fn add_needs_remove_first(self) -> bool {
        !matches!(self, Self::Codex)
    }

    /// The configuration to add by hand when the client's command line is not
    /// available.
    fn manual_snippet(self, exe: &str) -> String {
        let command = json_string(exe);
        match self {
            Self::Claude => format!(
                "\"mcpServers\": {{ \"{SERVER_NAME}\": {{ \"type\": \"stdio\", \"command\": {command}, \"args\": [\"{SERVE_ARG}\"] }} }}"
            ),
            Self::Codex => format!(
                "[mcp_servers.{SERVER_NAME}]\ncommand = {command}\nargs = [\"{SERVE_ARG}\"]"
            ),
            Self::Gemini => format!(
                "\"mcpServers\": {{ \"{SERVER_NAME}\": {{ \"command\": {command}, \"args\": [\"{SERVE_ARG}\"] }} }}"
            ),
        }
    }

    /// Reads the current CretLink registration from the configuration file.
    fn read_registration(self, context: &Context) -> Registered {
        let Some(path) = self.config_path(context) else {
            return Registered::Absent;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Registered::Absent,
            Err(err) => {
                return Registered::Unreadable(format!("cannot read {}: {err}", path.display()));
            }
        };
        match self {
            Self::Claude => read_json_registration(&text, false),
            Self::Codex => read_codex_registration(&text),
            Self::Gemini => read_json_registration(&text, true),
        }
    }
}

/// A command and arguments a client starts for a registered MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// The program the client starts.
    pub command: String,
    /// The arguments it passes.
    pub args: Vec<String>,
}

impl Registration {
    fn display(&self) -> String {
        let mut parts = vec![self.command.clone()];
        parts.extend(self.args.iter().cloned());
        display_command(&parts)
    }
}

/// The state of the CretLink registration in one client's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registered {
    /// No `cretlink` entry.
    Absent,
    /// A `cretlink` entry with this command.
    Present(Registration),
    /// A `cretlink` entry, or a file, this command could not read.
    Unreadable(String),
}

/// Where to look for the clients and their configuration.
#[derive(Debug, Clone)]
pub struct Context {
    /// The absolute path of the running `cretlink` program.
    pub exe: PathBuf,
    /// The user's home directory.
    pub home: Option<PathBuf>,
    /// The directories of `PATH`, in order.
    pub path: Vec<PathBuf>,
    /// Executable extensions to try on Windows (`PATHEXT`); `[""]` elsewhere.
    pub path_ext: Vec<String>,
    /// `CLAUDE_CONFIG_DIR`, which moves Claude Code's configuration.
    pub claude_config_dir: Option<PathBuf>,
    /// `CODEX_HOME`, which moves Codex CLI's configuration.
    pub codex_home: Option<PathBuf>,
}

impl Context {
    /// Builds the context from this process's environment.
    #[must_use]
    pub fn from_env(exe: PathBuf) -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        let home = var("HOME")
            .or_else(|| var("USERPROFILE"))
            .map(PathBuf::from);
        let path = var("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        let path_ext = if cfg!(windows) {
            let value = var("PATHEXT")
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_owned());
            value
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(str::to_owned)
                .collect()
        } else {
            vec![String::new()]
        };
        Self {
            exe,
            home,
            path,
            path_ext,
            claude_config_dir: var("CLAUDE_CONFIG_DIR").map(PathBuf::from),
            codex_home: var("CODEX_HOME").map(PathBuf::from),
        }
    }

    /// Finds a client's program on `PATH`.
    fn find_program(&self, name: &str) -> Option<PathBuf> {
        self.path.iter().find_map(|dir| {
            self.path_ext
                .iter()
                .map(|ext| dir.join(format!("{name}{ext}")))
                .find(|candidate| is_executable(candidate))
        })
    }

    fn exe_string(&self) -> String {
        self.exe.to_string_lossy().into_owned()
    }

    /// Whether a registration starts this program with the `mcp` argument.
    fn is_this_program(&self, registration: &Registration) -> bool {
        let same_path = if cfg!(windows) {
            registration
                .command
                .eq_ignore_ascii_case(&self.exe_string())
        } else {
            registration.command == self.exe_string()
        };
        same_path && registration.args == [SERVE_ARG]
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The result of running a client command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// Whether the command exited successfully.
    pub success: bool,
    /// What it wrote to stdout and stderr, for reporting a failure.
    pub output: String,
}

/// Runs a client's command line.
pub trait Runner {
    /// Runs `program` with `args` and waits for it.
    ///
    /// # Errors
    ///
    /// Returns an error when the program cannot be started.
    fn run(&self, program: &Path, args: &[String]) -> io::Result<RunOutput>;
}

/// Runs the real client programs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &Path, args: &[String]) -> io::Result<RunOutput> {
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(RunOutput {
            success: output.status.success(),
            output: text,
        })
    }
}

/// Runs a setup command and writes its report to `out`.
///
/// Returns `Ok(true)` when every requested client ended in the wanted state, or
/// was absent without being asked for by name.
///
/// # Errors
///
/// Returns an error only when writing the report fails.
pub fn execute(
    context: &Context,
    runner: &dyn Runner,
    action: Action,
    options: &SetupOptions,
    out: &mut dyn Write,
) -> io::Result<bool> {
    let explicit = !options.clients.is_empty();
    let clients: &[Client] = if explicit {
        &options.clients
    } else {
        &Client::ALL
    };
    let exe = context.exe_string();
    if action != Action::Status {
        writeln!(out, "CretLink program: {exe}")?;
        if options.dry_run {
            writeln!(out, "Dry run: nothing is changed.")?;
        }
    }
    let mut ok = true;
    for &client in clients {
        let program = context.find_program(client.program());
        let registered = client.read_registration(context);
        ok &= match action {
            Action::Status => {
                report_status(context, client, program.as_deref(), &registered, out)?;
                true
            }
            Action::Install => install(
                context,
                runner,
                client,
                program.as_deref(),
                &registered,
                options.dry_run,
                explicit,
                out,
            )?,
            Action::Uninstall => uninstall(
                context,
                runner,
                client,
                program.as_deref(),
                &registered,
                options.dry_run,
                explicit,
                out,
            )?,
        };
    }
    Ok(ok)
}

fn report_status(
    context: &Context,
    client: Client,
    program: Option<&Path>,
    registered: &Registered,
    out: &mut dyn Write,
) -> io::Result<()> {
    let presence = match program {
        Some(path) => format!("found at {}", path.display()),
        None => format!("'{}' not found on PATH", client.program()),
    };
    writeln!(out, "{}: {presence}", client.label())?;
    let state = match registered {
        Registered::Absent => "CretLink is not registered".to_owned(),
        Registered::Present(registration) if context.is_this_program(registration) => {
            format!(
                "CretLink is registered with this program: {}",
                registration.display()
            )
        }
        Registered::Present(registration) => format!(
            "CretLink is registered with another program: {} (running 'mcp install' again updates it)",
            registration.display()
        ),
        Registered::Unreadable(reason) => {
            format!("the registration could not be read: {reason}")
        }
    };
    writeln!(out, "  {state}")?;
    if let Some(path) = client.config_path(context) {
        writeln!(out, "  configuration: {}", path.display())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn install(
    context: &Context,
    runner: &dyn Runner,
    client: Client,
    program: Option<&Path>,
    registered: &Registered,
    dry_run: bool,
    explicit: bool,
    out: &mut dyn Write,
) -> io::Result<bool> {
    let exe = context.exe_string();
    let Some(program) = program else {
        writeln!(
            out,
            "{}: '{}' not found on PATH; skipped.",
            client.label(),
            client.program()
        )?;
        let location = client.config_path(context).map_or_else(
            || "its configuration".to_owned(),
            |path| path.display().to_string(),
        );
        writeln!(out, "  To register by hand, add to {location}:")?;
        for line in client.manual_snippet(&exe).lines() {
            writeln!(out, "    {line}")?;
        }
        return Ok(!explicit);
    };
    let mut steps = Vec::new();
    match registered {
        Registered::Present(registration) if context.is_this_program(registration) => {
            writeln!(
                out,
                "{}: already registered with this program; nothing to do.",
                client.label()
            )?;
            return Ok(true);
        }
        Registered::Present(registration) => {
            writeln!(
                out,
                "{}: replacing the registration that starts {}.",
                client.label(),
                registration.display()
            )?;
            if client.add_needs_remove_first() {
                steps.push(client.remove_args());
            }
        }
        Registered::Unreadable(reason) => {
            writeln!(
                out,
                "{}: the current registration could not be read ({reason}); registering again.",
                client.label()
            )?;
            if client.add_needs_remove_first() {
                steps.push(client.remove_args());
            }
        }
        Registered::Absent => {
            writeln!(
                out,
                "{}: registering the '{SERVER_NAME}' MCP server.",
                client.label()
            )?;
        }
    }
    steps.push(client.add_args(&exe));
    run_steps(runner, client, program, &steps, dry_run, out)
}

#[allow(clippy::too_many_arguments)]
fn uninstall(
    context: &Context,
    runner: &dyn Runner,
    client: Client,
    program: Option<&Path>,
    registered: &Registered,
    dry_run: bool,
    explicit: bool,
    out: &mut dyn Write,
) -> io::Result<bool> {
    if *registered == Registered::Absent {
        writeln!(
            out,
            "{}: CretLink is not registered; nothing to do.",
            client.label()
        )?;
        return Ok(true);
    }
    let Some(program) = program else {
        let location = client.config_path(context).map_or_else(
            || "its configuration".to_owned(),
            |path| path.display().to_string(),
        );
        writeln!(
            out,
            "{}: '{}' not found on PATH; remove the '{SERVER_NAME}' entry from {location} by hand.",
            client.label(),
            client.program()
        )?;
        return Ok(!explicit);
    };
    writeln!(
        out,
        "{}: removing the '{SERVER_NAME}' MCP server.",
        client.label()
    )?;
    run_steps(
        runner,
        client,
        program,
        &[client.remove_args()],
        dry_run,
        out,
    )
}

fn run_steps(
    runner: &dyn Runner,
    client: Client,
    program: &Path,
    steps: &[Vec<String>],
    dry_run: bool,
    out: &mut dyn Write,
) -> io::Result<bool> {
    for args in steps {
        let mut shown = vec![client.program().to_owned()];
        shown.extend(args.iter().cloned());
        writeln!(out, "  $ {}", display_command(&shown))?;
        if dry_run {
            continue;
        }
        match runner.run(program, args) {
            Ok(result) if result.success => {}
            Ok(result) => {
                writeln!(out, "  failed:")?;
                for line in result.output.lines().filter(|line| !line.trim().is_empty()) {
                    writeln!(out, "    {line}")?;
                }
                return Ok(false);
            }
            Err(err) => {
                writeln!(out, "  could not run {}: {err}", program.display())?;
                return Ok(false);
            }
        }
    }
    if !dry_run {
        writeln!(out, "  done.")?;
    }
    Ok(true)
}

/// Shows a command line, quoting the arguments a shell would split.
fn display_command(parts: &[String]) -> String {
    let mut shown = String::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            shown.push(' ');
        }
        if part.is_empty()
            || part
                .chars()
                .any(|c| c.is_whitespace() || "\"'$&|;<>()".contains(c))
        {
            let _ = write!(shown, "\"{}\"", part.replace('"', "\\\""));
        } else {
            shown.push_str(part);
        }
    }
    shown
}

fn json_string(value: &str) -> String {
    Value::String(value.to_owned()).to_string()
}

/// Reads `mcpServers.cretlink` from a Claude Code or Gemini CLI JSON file.
/// Gemini CLI accepts comments in its settings, so they are stripped first.
fn read_json_registration(text: &str, allow_comments: bool) -> Registered {
    let parsed = if allow_comments {
        serde_json::from_str::<Value>(&strip_json_comments(text))
    } else {
        serde_json::from_str::<Value>(text)
    };
    let root = match parsed {
        Ok(root) => root,
        Err(err) => return Registered::Unreadable(format!("invalid JSON: {err}")),
    };
    let Some(entry) = root
        .get("mcpServers")
        .and_then(|servers| servers.get(SERVER_NAME))
    else {
        return Registered::Absent;
    };
    let Some(command) = entry.get("command").and_then(Value::as_str) else {
        return Registered::Unreadable(format!("the '{SERVER_NAME}' entry has no command"));
    };
    let args = match entry.get("args") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            let strings: Option<Vec<String>> = items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect();
            match strings {
                Some(strings) => strings,
                None => {
                    return Registered::Unreadable(format!(
                        "the '{SERVER_NAME}' entry has non-string arguments"
                    ));
                }
            }
        }
        Some(_) => {
            return Registered::Unreadable(format!(
                "the '{SERVER_NAME}' entry has malformed arguments"
            ));
        }
    };
    Registered::Present(Registration {
        command: command.to_owned(),
        args,
    })
}

/// Removes `//` and `/* */` comments outside JSON strings.
fn strip_json_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Reads `[mcp_servers.cretlink]` from a Codex CLI `config.toml`.
///
/// This reads the table form that `codex mcp add` writes: a
/// `[mcp_servers.cretlink]` header followed by `command` and `args` keys with
/// basic or literal strings. Any other spelling of a `cretlink` server is
/// reported as unreadable rather than guessed at.
fn read_codex_registration(text: &str) -> Registered {
    let wanted = format!("mcp_servers.{SERVER_NAME}");
    let mut lines = text.lines();
    let mut table = String::new();
    let mut found = false;
    let mut other_form = false;
    let mut command = None;
    let mut args = None;
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if let Some(name) = table_name(trimmed) {
            if found {
                break;
            }
            found = name == wanted;
            other_form |= !found && name.starts_with(&format!("{wanted}."));
            table = name;
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if !found {
            // `cretlink = { … }` under `[mcp_servers]`, or a dotted
            // `mcp_servers.cretlink.command = …` key elsewhere.
            if !trimmed.starts_with('#') {
                let key = normalize_key(key);
                let full = if table.is_empty() {
                    key
                } else {
                    format!("{table}.{key}")
                };
                other_form |= full == wanted || full.starts_with(&format!("{wanted}."));
            }
            continue;
        }
        match key.trim() {
            "command" => match parse_toml_string(value.trim()) {
                Some((text, _)) => command = Some(text),
                None => return Registered::Unreadable("unsupported 'command' value".to_owned()),
            },
            "args" => {
                let mut array = value.trim().to_owned();
                while !array_closed(&array) {
                    match lines.next() {
                        Some(next) => {
                            array.push('\n');
                            array.push_str(next);
                        }
                        None => break,
                    }
                }
                match parse_toml_string_array(&array) {
                    Some(items) => args = Some(items),
                    None => return Registered::Unreadable("unsupported 'args' value".to_owned()),
                }
            }
            _ => {}
        }
    }
    if found {
        return match command {
            Some(command) => Registered::Present(Registration {
                command,
                args: args.unwrap_or_default(),
            }),
            None => Registered::Unreadable(format!("the '{SERVER_NAME}' table has no command")),
        };
    }
    if other_form {
        Registered::Unreadable(format!(
            "a '{SERVER_NAME}' server is configured in a form this command does not read"
        ))
    } else {
        Registered::Absent
    }
}

/// The normalized name of a `[table]` header line, or `None` for any other line
/// (including `[[array]]` headers).
fn table_name(line: &str) -> Option<String> {
    let inner = line.strip_prefix('[')?;
    if inner.starts_with('[') {
        return None;
    }
    let end = inner.find(']')?;
    Some(normalize_key(&inner[..end]))
}

/// Normalizes a dotted TOML key: trims each part and drops its quotes.
fn normalize_key(key: &str) -> String {
    let parts: Vec<&str> = key
        .split('.')
        .map(|part| part.trim().trim_matches('"').trim_matches('\''))
        .collect();
    parts.join(".")
}

/// Whether a TOML array value has its closing bracket outside any string.
fn array_closed(value: &str) -> bool {
    let mut rest = value.trim_start().strip_prefix('[').unwrap_or(value);
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if rest.starts_with('#') {
            match rest.find('\n') {
                Some(end) => rest = &rest[end..],
                None => return false,
            }
            continue;
        }
        if rest.starts_with(']') {
            return true;
        }
        match parse_toml_string(rest) {
            Some((_, after)) => rest = after,
            None => return false,
        }
    }
}

/// Parses an array of TOML strings, such as `["mcp"]`.
fn parse_toml_string_array(value: &str) -> Option<Vec<String>> {
    let mut rest = value.trim_start().strip_prefix('[')?;
    let mut items = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if rest.starts_with('#') {
            rest = &rest[rest.find('\n')?..];
            continue;
        }
        if rest.starts_with(']') {
            return Some(items);
        }
        let (item, after) = parse_toml_string(rest)?;
        items.push(item);
        rest = after;
    }
}

/// Parses one TOML basic (`"…"`) or literal (`'…'`) single-line string at the
/// start of `value`, returning it and the text after it. Multi-line strings are
/// not supported.
fn parse_toml_string(value: &str) -> Option<(String, &str)> {
    if value.starts_with("\"\"\"") || value.starts_with("'''") {
        return None;
    }
    if let Some(rest) = value.strip_prefix('\'') {
        let end = rest.find('\'')?;
        return Some((rest[..end].to_owned(), &rest[end + 1..]));
    }
    let rest = value.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((index, c)) = chars.next() {
        match c {
            '"' => return Some((out, &rest[index + 1..])),
            '\\' => {
                let (_, escape) = chars.next()?;
                match escape {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'e' => out.push('\u{1b}'),
                    'u' | 'U' => {
                        let len = if escape == 'u' { 4 } else { 8 };
                        let mut hex = String::new();
                        for _ in 0..len {
                            hex.push(chars.next()?.1);
                        }
                        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                    }
                    _ => return None,
                }
            }
            '\n' => return None,
            _ => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records the commands it is asked to run and answers with a fixed result.
    struct Recorder {
        calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
        succeed: bool,
    }

    impl Recorder {
        fn new(succeed: bool) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                succeed,
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls
                .borrow()
                .iter()
                .map(|(_, args)| args.clone())
                .collect()
        }
    }

    impl Runner for Recorder {
        fn run(&self, program: &Path, args: &[String]) -> io::Result<RunOutput> {
            self.calls
                .borrow_mut()
                .push((program.to_path_buf(), args.to_vec()));
            Ok(RunOutput {
                success: self.succeed,
                output: if self.succeed {
                    String::new()
                } else {
                    "Error: something went wrong\n".to_owned()
                },
            })
        }
    }

    struct Fixture {
        dir: PathBuf,
        context: Context,
    }

    impl Fixture {
        /// A temporary home with a `bin` directory holding the given fake
        /// client programs.
        fn new(name: &str, programs: &[&str]) -> Self {
            let dir =
                std::env::temp_dir().join(format!("cretlink-setup-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let bin = dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap_or_else(|err| panic!("mkdir: {err}"));
            let ext = if cfg!(windows) { ".cmd" } else { "" };
            for program in programs {
                let path = bin.join(format!("{program}{ext}"));
                std::fs::write(&path, "").unwrap_or_else(|err| panic!("write: {err}"));
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .unwrap_or_else(|err| panic!("chmod: {err}"));
                }
            }
            let context = Context {
                exe: dir.join("app").join("cretlink"),
                home: Some(dir.clone()),
                path: vec![bin],
                path_ext: vec![ext.to_owned()],
                claude_config_dir: None,
                codex_home: None,
            };
            Self { dir, context }
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.dir.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap_or_else(|err| panic!("mkdir: {err}"));
            }
            std::fs::write(path, text).unwrap_or_else(|err| panic!("write: {err}"));
        }

        fn exe(&self) -> String {
            self.context.exe_string()
        }

        fn execute(
            &self,
            runner: &Recorder,
            action: Action,
            options: &SetupOptions,
        ) -> (bool, String) {
            let mut out = Vec::new();
            let ok = execute(&self.context, runner, action, options, &mut out)
                .unwrap_or_else(|err| panic!("execute: {err}"));
            (ok, String::from_utf8_lossy(&out).into_owned())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn install_registers_with_every_present_client() {
        let fixture = Fixture::new("install-all", &["claude", "codex", "gemini"]);
        let runner = Recorder::new(true);
        let (ok, report) = fixture.execute(&runner, Action::Install, &SetupOptions::default());
        assert!(ok, "{report}");
        let exe = fixture.exe();
        assert_eq!(
            runner.calls(),
            vec![
                strings(&[
                    "mcp", "add", "--scope", "user", "cretlink", "--", &exe, "mcp"
                ]),
                strings(&["mcp", "add", "cretlink", "--", &exe, "mcp"]),
                strings(&["mcp", "add", "--scope", "user", "cretlink", &exe, "mcp"]),
            ]
        );
        assert!(report.contains("$ claude mcp add --scope user cretlink --"));
        assert!(report.contains("done."));
    }

    #[test]
    fn install_is_idempotent() {
        let fixture = Fixture::new("idempotent", &["claude", "codex", "gemini"]);
        let exe = fixture.exe();
        let json_exe = json_string(&exe);
        fixture.write(
            ".claude.json",
            &format!(
                r#"{{"numStartups": 3, "mcpServers": {{"cretlink": {{"type": "stdio", "command": {json_exe}, "args": ["mcp"], "env": {{}}}}}}}}"#
            ),
        );
        fixture.write(
            ".codex/config.toml",
            &format!(
                "model = \"x\"\n\n[mcp_servers.cretlink]\ncommand = {json_exe}\nargs = [\"mcp\"]\n"
            ),
        );
        fixture.write(
            ".gemini/settings.json",
            &format!(
                "{{\n  // user settings\n  \"mcpServers\": {{ \"cretlink\": {{ \"command\": {json_exe}, \"args\": [\"mcp\"] }} }}\n}}\n"
            ),
        );
        let runner = Recorder::new(true);
        let (ok, report) = fixture.execute(&runner, Action::Install, &SetupOptions::default());
        assert!(ok, "{report}");
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert_eq!(report.matches("already registered").count(), 3, "{report}");
    }

    #[test]
    fn install_replaces_a_registration_of_a_moved_program() {
        let fixture = Fixture::new("moved", &["claude", "codex"]);
        fixture.write(
            ".claude.json",
            r#"{"mcpServers": {"cretlink": {"command": "/old/cretlink", "args": ["mcp"]}}}"#,
        );
        fixture.write(
            ".codex/config.toml",
            "[mcp_servers.cretlink]\ncommand = '/old/cretlink'\nargs = [\n  \"mcp\",\n]\n",
        );
        let runner = Recorder::new(true);
        let options = SetupOptions {
            clients: vec![Client::Claude, Client::Codex],
            dry_run: false,
        };
        let (ok, report) = fixture.execute(&runner, Action::Install, &options);
        assert!(ok, "{report}");
        let exe = fixture.exe();
        assert_eq!(
            runner.calls(),
            vec![
                strings(&["mcp", "remove", "--scope", "user", "cretlink"]),
                strings(&[
                    "mcp", "add", "--scope", "user", "cretlink", "--", &exe, "mcp"
                ]),
                // Codex replaces the entry in place.
                strings(&["mcp", "add", "cretlink", "--", &exe, "mcp"]),
            ]
        );
        assert_eq!(
            report
                .matches("replacing the registration that starts /old/cretlink mcp")
                .count(),
            2,
            "{report}"
        );
    }

    #[test]
    fn dry_run_changes_nothing() {
        let fixture = Fixture::new("dry-run", &["claude"]);
        let runner = Recorder::new(true);
        let options = SetupOptions {
            clients: vec![Client::Claude],
            dry_run: true,
        };
        let (ok, report) = fixture.execute(&runner, Action::Install, &options);
        assert!(ok);
        assert!(runner.calls().is_empty());
        assert!(report.contains("Dry run"));
        assert!(report.contains("$ claude mcp add"));
        assert!(!report.contains("done."));
    }

    #[test]
    fn absent_clients_get_a_snippet_and_fail_only_when_named() {
        let fixture = Fixture::new("absent", &["claude"]);
        let runner = Recorder::new(true);
        let (ok, report) = fixture.execute(&runner, Action::Install, &SetupOptions::default());
        assert!(ok, "{report}");
        assert!(report.contains("Codex CLI: 'codex' not found on PATH; skipped."));
        assert!(report.contains("[mcp_servers.cretlink]"));
        assert!(report.contains("Gemini CLI: 'gemini' not found on PATH; skipped."));
        assert_eq!(runner.calls().len(), 1);

        let named = SetupOptions {
            clients: vec![Client::Gemini],
            dry_run: false,
        };
        let (ok, _) = fixture.execute(&runner, Action::Install, &named);
        assert!(!ok);
    }

    #[test]
    fn a_failing_client_command_is_reported() {
        let fixture = Fixture::new("failing", &["claude"]);
        let runner = Recorder::new(false);
        let options = SetupOptions {
            clients: vec![Client::Claude],
            dry_run: false,
        };
        let (ok, report) = fixture.execute(&runner, Action::Install, &options);
        assert!(!ok);
        assert!(report.contains("failed:"));
        assert!(report.contains("something went wrong"));
    }

    #[test]
    fn uninstall_removes_only_existing_registrations() {
        let fixture = Fixture::new("uninstall", &["claude", "codex", "gemini"]);
        fixture.write(
            ".claude.json",
            r#"{"mcpServers": {"other": {"command": "x"}, "cretlink": {"command": "/a/cretlink", "args": ["mcp"]}}}"#,
        );
        fixture.write(
            ".codex/config.toml",
            "[mcp_servers.other]\ncommand = \"x\"\n",
        );
        let runner = Recorder::new(true);
        let (ok, report) = fixture.execute(&runner, Action::Uninstall, &SetupOptions::default());
        assert!(ok, "{report}");
        assert_eq!(
            runner.calls(),
            vec![strings(&["mcp", "remove", "--scope", "user", "cretlink"])]
        );
        assert_eq!(report.matches("not registered; nothing to do").count(), 2);
    }

    #[test]
    fn status_reports_each_client() {
        let fixture = Fixture::new("status", &["claude", "codex"]);
        let exe = fixture.exe();
        fixture.write(
            ".claude.json",
            &format!(
                r#"{{"mcpServers": {{"cretlink": {{"command": {}, "args": ["mcp"]}}}}}}"#,
                json_string(&exe)
            ),
        );
        fixture.write(
            ".codex/config.toml",
            "[mcp_servers.cretlink]\ncommand = \"/elsewhere/cretlink\"\nargs = [\"mcp\"]\n",
        );
        let runner = Recorder::new(true);
        let (ok, report) = fixture.execute(&runner, Action::Status, &SetupOptions::default());
        assert!(ok);
        assert!(runner.calls().is_empty());
        assert!(
            report.contains("CretLink is registered with this program"),
            "{report}"
        );
        assert!(report.contains("registered with another program: /elsewhere/cretlink mcp"));
        assert!(report.contains("Gemini CLI: 'gemini' not found on PATH"));
        assert!(report.contains("CretLink is not registered"));
    }

    #[test]
    fn config_locations_follow_the_client_overrides() {
        let mut fixture = Fixture::new("overrides", &[]);
        fixture.context.claude_config_dir = Some(PathBuf::from("/c"));
        fixture.context.codex_home = Some(PathBuf::from("/x"));
        assert_eq!(
            Client::Claude.config_path(&fixture.context),
            Some(PathBuf::from("/c/.claude.json"))
        );
        assert_eq!(
            Client::Codex.config_path(&fixture.context),
            Some(PathBuf::from("/x/config.toml"))
        );
    }

    #[test]
    fn codex_reader_handles_quoting_and_other_forms() {
        let quoted = "[mcp_servers.\"cretlink\"]\ncommand = \"C:\\\\Users\\\\Ann Lee\\\\cretlink-cli.exe\" # note\nargs = ['mcp']\n[other]\ncommand = \"no\"\n";
        assert_eq!(
            read_codex_registration(quoted),
            Registered::Present(Registration {
                command: r"C:\Users\Ann Lee\cretlink-cli.exe".to_owned(),
                args: strings(&["mcp"]),
            })
        );
        let inline = "[mcp_servers]\ncretlink = { command = \"x\" }\n";
        assert!(matches!(
            read_codex_registration(inline),
            Registered::Unreadable(_)
        ));
        let dotted = "mcp_servers.cretlink.command = \"x\"\n";
        assert!(matches!(
            read_codex_registration(dotted),
            Registered::Unreadable(_)
        ));
        let other = "[mcp_servers.cretlinker]\ncommand = \"x\"\n";
        assert_eq!(read_codex_registration(other), Registered::Absent);
        assert_eq!(
            read_codex_registration("# mcp_servers.cretlink\n"),
            Registered::Absent
        );
        assert_eq!(read_codex_registration(""), Registered::Absent);
    }

    #[test]
    fn json_reader_rejects_malformed_entries() {
        assert!(matches!(
            read_json_registration("{", false),
            Registered::Unreadable(_)
        ));
        assert!(matches!(
            read_json_registration(r#"{"mcpServers": {"cretlink": {"args": []}}}"#, false),
            Registered::Unreadable(_)
        ));
        assert_eq!(
            read_json_registration(r#"{"mcpServers": {}}"#, false),
            Registered::Absent
        );
    }

    #[test]
    fn comments_inside_strings_are_kept() {
        let text = "{ \"a\": \"http://x/*y*/\", /* c */ \"b\": 1 // end\n}";
        let value: Value = serde_json::from_str(&strip_json_comments(text))
            .unwrap_or_else(|err| panic!("parse: {err}"));
        assert_eq!(value["a"], "http://x/*y*/");
        assert_eq!(value["b"], 1);
    }

    #[test]
    fn displayed_commands_quote_spaces() {
        assert_eq!(
            display_command(&strings(&["claude", "/opt/my dir/cretlink", "mcp"])),
            "claude \"/opt/my dir/cretlink\" mcp"
        );
    }
}
