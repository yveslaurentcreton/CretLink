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
//! there would run with the customer process's administrator rights. The
//! Windows folder is asked from the system and not read from `%SystemRoot%`,
//! which whoever starts the program can choose (ST-M006-020). On Linux (the
//! technician role only) `xdg-open` is found on `PATH`: it lives in different
//! places across distributions, and `PATH` there is the user's own.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The program that opens files and folders on this platform.
///
/// # Errors
///
/// On Windows, when the system does not name its Windows folder. Nothing is
/// started then.
pub(crate) fn program() -> io::Result<PathBuf> {
    program_for(cretlink_support_agent::windows_folder())
}

/// [`program`], with the Windows folder as the system names it.
fn program_for(windows: Option<PathBuf>) -> io::Result<PathBuf> {
    if cfg!(windows) {
        windows
            .map(|folder| folder.join("explorer.exe"))
            .ok_or_else(|| io::Error::other("the Windows folder could not be read"))
    } else if cfg!(target_os = "macos") {
        Ok(PathBuf::from("/usr/bin/open"))
    } else {
        Ok(PathBuf::from("xdg-open"))
    }
}

/// The opener invocation for `path`: the program and the path as its only
/// argument, with no inherited standard streams.
///
/// # Errors
///
/// See [`program`].
pub(crate) fn command(path: &Path) -> io::Result<Command> {
    let mut command = Command::new(program()?);
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
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
    let mut child = command(path)?.spawn()?;
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
        let program = program().unwrap_or_default();
        let Ok(command) = command(path) else {
            panic!("the opener of this platform is known");
        };
        assert_eq!(command.get_program(), program.as_os_str());
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec![path.as_os_str()]);
        let name = program
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
        let program = program_for(Some(PathBuf::from(r"D:\Win"))).unwrap_or_default();
        if cfg!(windows) {
            assert_eq!(program, Path::new(r"D:\Win").join("explorer.exe"));
            assert!(program.is_absolute());
            // Without the system's answer nothing is started: no folder is
            // guessed, and none is read from the environment.
            assert!(program_for(None).is_err());
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
