//! Opens a local file or folder with the platform's own opener (ST-M009-012).
//!
//! `explorer.exe` on Windows, `open` on macOS, `xdg-open` elsewhere, started
//! directly with the path as its single argument — never through a shell, so
//! nothing in the path is interpreted. Callers pass only paths they have
//! validated themselves (an audit file the history listed, a report this
//! process saved).
//!
//! The opener is started by its absolute path on Windows and macOS. Windows
//! looks for a bare program name in the running program's own folder first —
//! for a customer that is often `Downloads` — so a planted `explorer.exe`
//! there would run with the customer process's administrator rights. On Linux
//! (the technician role only) `xdg-open` is found on `PATH`: it lives in
//! different places across distributions, and `PATH` there is the user's own.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The program that opens files and folders on this platform.
#[must_use]
pub(crate) fn program() -> PathBuf {
    program_for(std::env::var_os("SystemRoot"))
}

/// [`program`], with the Windows directory given (`%SystemRoot%`, falling
/// back to `C:\Windows` as the installer's removal helper does).
fn program_for(system_root: Option<OsString>) -> PathBuf {
    if cfg!(windows) {
        system_root
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("explorer.exe")
    } else if cfg!(target_os = "macos") {
        PathBuf::from("/usr/bin/open")
    } else {
        PathBuf::from("xdg-open")
    }
}

/// The opener invocation for `path`: the program and the path as its only
/// argument, with no inherited standard streams.
#[must_use]
pub(crate) fn command(path: &Path) -> Command {
    let mut command = Command::new(program());
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Opens `path`, which must be absolute. The opener runs on its own; a thread
/// reaps it so no zombie stays behind. `explorer` reports a non-zero status
/// even when it opened the path, so only a failure to start is an error.
///
/// # Errors
///
/// When `path` is relative, or the opener cannot be started.
pub(crate) fn open(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only an absolute path can be opened",
        ));
    }
    let mut child = command(path).spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_the_single_argument_and_no_shell_is_involved() {
        let path = Path::new("/tmp/a b; rm -rf ~ & echo.html");
        let command = command(path);
        assert_eq!(command.get_program(), program().as_os_str());
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec![path.as_os_str()]);
        let name = program();
        let name = name
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        assert!(
            ![
                "sh",
                "bash",
                "cmd",
                "cmd.exe",
                "powershell",
                "powershell.exe"
            ]
            .contains(&name)
        );
    }

    #[test]
    fn the_opener_is_started_by_its_absolute_path_on_windows_and_macos() {
        let program = program_for(Some(OsString::from(r"D:\Win")));
        if cfg!(windows) {
            assert_eq!(program, Path::new(r"D:\Win").join("explorer.exe"));
            assert_eq!(
                program_for(None),
                Path::new(r"C:\Windows").join("explorer.exe")
            );
            assert!(program.is_absolute());
        } else if cfg!(target_os = "macos") {
            assert_eq!(program, Path::new("/usr/bin/open"));
        } else {
            // Linux: found on the user's own PATH (see the module note).
            assert_eq!(program, Path::new("xdg-open"));
        }
    }

    #[test]
    fn a_relative_path_is_refused() {
        assert!(open(Path::new("relative/report.html")).is_err());
    }
}
