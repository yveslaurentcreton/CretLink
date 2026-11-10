//! Process plumbing for the one CretLink application on Windows (ST-M009-001,
//! ADR-0018): a window-less executable that still talks to the terminal that
//! started it, and the elevated relaunch of the customer role.
//!
//! `cretlink.exe` is built for the Windows GUI subsystem so a double-click opens
//! no console window. MCP clients start `cretlink mcp` with redirected standard
//! handles, which such a process inherits and uses normally. A subcommand
//! started from a terminal has no standard handles, so
//! [`attach_parent_console`] attaches to the terminal's console and points
//! stdout and stderr at it. A terminal does not wait for a GUI-subsystem
//! process, so an *interactive* command (`shell`) cannot share the console with
//! the prompt; the console-subsystem `cretlink-cli` serves that.
//!
//! The FFI lives in the `win` adapter; with `install::win` it is the only
//! `unsafe` code of this crate.

use std::io;
use std::path::Path;

#[cfg(windows)]
mod win;

/// How the process's standard output relates to a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleAttach {
    /// The standard handles were already usable: inherited from a console or
    /// redirected to pipes or files (always the case outside Windows, and for
    /// a console-subsystem build).
    Inherited,
    /// The process attached to the console of the terminal that started it.
    Attached,
    /// There is no terminal to write to, for example after a double-click.
    None,
}

/// Makes stdout and stderr reach the terminal that started this process, when
/// the process has no standard handles of its own (a Windows GUI-subsystem
/// build started from a terminal). Never replaces handles that are already
/// usable, so redirected pipes (an MCP client) are left alone.
#[must_use]
pub fn attach_parent_console() -> ConsoleAttach {
    #[cfg(windows)]
    {
        win::attach_parent_console()
    }
    #[cfg(not(windows))]
    {
        ConsoleAttach::Inherited
    }
}

/// Tells the user something went wrong when there may be no window or terminal
/// to say it in: a native message box on Windows, stderr elsewhere.
pub fn alert(title: &str, message: &str) {
    #[cfg(windows)]
    {
        win::alert(title, message);
    }
    #[cfg(not(windows))]
    {
        eprintln!("{title}: {message}");
    }
}

/// Asks a yes-or-no question in a native message box (Windows), for a
/// GUI-subsystem program that has no terminal to ask in (`cretlink uninstall`
/// from the Windows uninstall entry, ST-M009-006). "No" is the default
/// button. Elsewhere there is no such box and the answer is no.
#[must_use]
pub fn confirm(title: &str, message: &str) -> bool {
    #[cfg(windows)]
    {
        win::confirm(title, message)
    }
    #[cfg(not(windows))]
    {
        let _ = (title, message);
        false
    }
}

/// Tells the user how something went in a native message box (Windows), or
/// on stdout elsewhere.
pub fn inform(title: &str, message: &str) {
    #[cfg(windows)]
    {
        win::inform(title, message);
    }
    #[cfg(not(windows))]
    {
        println!("{title}: {message}");
    }
}

/// The outcome of asking Windows to start this program elevated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// The user approved UAC and the elevated process started.
    Started,
    /// The user declined the UAC prompt; nothing started.
    Declined,
}

/// Starts `exe` again with `args`, elevated through the UAC prompt
/// (`ShellExecuteExW` with the `runas` verb). The prompt is the visible
/// elevation REQ-F-002 asks for; declining it is reported as
/// [`Elevation::Declined`], not as an error.
///
/// # Errors
///
/// Returns an error when Windows cannot start the program for another reason,
/// and on every other operating system.
pub fn relaunch_elevated(exe: &Path, args: &[String]) -> io::Result<Elevation> {
    #[cfg(windows)]
    {
        win::relaunch_elevated(exe, &windows_command_line(args))
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, args);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "elevation through UAC exists only on Windows",
        ))
    }
}

/// Joins arguments into one Windows command-line string that
/// `CommandLineToArgvW` and the Microsoft C runtime split back into exactly
/// these arguments.
#[must_use]
pub fn windows_command_line(args: &[String]) -> String {
    let mut line = String::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        let needs_quotes = arg.is_empty() || arg.contains([' ', '\t', '\n', '\u{b}', '"']);
        if !needs_quotes {
            line.push_str(arg);
            continue;
        }
        line.push('"');
        let mut backslashes = 0usize;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    // Backslashes before a quote are escaped, then the quote.
                    line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    line.push('"');
                    backslashes = 0;
                }
                _ => {
                    line.extend(std::iter::repeat_n('\\', backslashes));
                    line.push(c);
                    backslashes = 0;
                }
            }
        }
        // Backslashes before the closing quote are escaped too.
        line.extend(std::iter::repeat_n('\\', backslashes * 2));
        line.push('"');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        windows_command_line(&owned)
    }

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(line(&["--role", "support"]), "--role support");
    }

    #[test]
    fn spaces_and_empty_arguments_are_quoted() {
        assert_eq!(
            line(&["C:\\Program Files\\x", ""]),
            "\"C:\\Program Files\\x\" \"\""
        );
    }

    #[test]
    fn quotes_and_trailing_backslashes_are_escaped() {
        // `a"b` → "a\"b"; `dir with\` → "dir with\\" so the quote stays closing.
        assert_eq!(line(&["a\"b"]), "\"a\\\"b\"");
        assert_eq!(line(&["dir with\\"]), "\"dir with\\\\\"");
        // Backslashes not followed by a quote stay as they are.
        assert_eq!(line(&["a\\\\b c"]), "\"a\\\\b c\"");
        // Backslashes before an embedded quote are doubled, plus one for the quote.
        assert_eq!(line(&["x\\\"y z"]), "\"x\\\\\\\"y z\"");
    }

    #[test]
    fn outside_windows_there_is_nothing_to_attach_or_elevate() {
        if cfg!(windows) {
            return;
        }
        assert_eq!(attach_parent_console(), ConsoleAttach::Inherited);
        let err = relaunch_elevated(Path::new("/bin/true"), &[])
            .err()
            .map(|err| err.kind());
        assert_eq!(err, Some(io::ErrorKind::Unsupported));
    }
}
