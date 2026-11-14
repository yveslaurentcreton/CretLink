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
//!
//! [`show_folder`] shows a folder in the technician's file manager, for the
//! copies the file explorer made (ST-M010-002). It starts the opener by its
//! full path on every system and never by a name that a search path
//! resolves: on Linux it looks for `xdg-open` in the system's own program
//! folders and starts nothing when it is in none of them.

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

/// The folders a Linux system keeps its own programs in, in the order they
/// are looked at for the opener.
const SYSTEM_PROGRAM_FOLDERS: [&str; 3] = ["/usr/bin", "/bin", "/usr/local/bin"];

/// The program that shows a folder in the file manager, by its full path:
/// the opener of [`program`] on Windows and macOS, and on Linux `xdg-open`
/// from the first of the system's program folders that holds it.
///
/// # Errors
///
/// When the system does not name its Windows folder, or no folder of the
/// system holds the opener. Nothing is started then.
pub(crate) fn file_manager() -> io::Result<PathBuf> {
    if cfg!(any(windows, target_os = "macos")) {
        return program();
    }
    in_system_folders("xdg-open", &SYSTEM_PROGRAM_FOLDERS)
}

/// The full path of the program `name` in the first of `folders` that holds
/// it as a file.
fn in_system_folders(name: &str, folders: &[&str]) -> io::Result<PathBuf> {
    folders
        .iter()
        .map(|folder| Path::new(folder).join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{name} was not found in the system's program folders"),
            )
        })
}

/// Shows `folder`, which must be an absolute path to a folder, in the file
/// manager. The opener is started by its full path with the folder as its
/// only argument, never through a shell, and runs on its own.
///
/// # Errors
///
/// When `folder` is relative or is no folder, when the opener is not found,
/// or when it cannot be started.
pub(crate) fn show_folder(folder: &Path) -> io::Result<()> {
    if !folder.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only an absolute path can be shown",
        ));
    }
    if !folder.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the folder is no longer there",
        ));
    }
    let mut child = Command::new(file_manager()?)
        .arg(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
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

    /// The file manager is started by a full path on every system, never by
    /// a name that a search path resolves (ST-M010-002).
    #[test]
    fn the_file_manager_is_named_by_its_full_path() {
        // Found or not, what is named is a full path: a bare name is never
        // handed to the system to look for.
        if let Ok(found) = file_manager() {
            assert!(found.is_absolute(), "{found:?}");
        }
        // On Linux the opener is looked for in the folders that are given,
        // and nowhere else.
        let here = std::env::temp_dir().join(format!("cretlink-opener-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&here);
        std::fs::create_dir_all(here.join("second")).unwrap_or_default();
        std::fs::write(here.join("second").join("xdg-open"), b"").unwrap_or_default();
        let first = here.join("first").display().to_string();
        let second = here.join("second").display().to_string();
        let found = in_system_folders("xdg-open", &[&first, &second]).unwrap_or_default();
        assert_eq!(found, here.join("second").join("xdg-open"));
        assert!(in_system_folders("xdg-open", &[&first]).is_err());
        assert!(in_system_folders("no-such-opener", &[&first, &second]).is_err());
        let _ = std::fs::remove_dir_all(&here);
    }

    #[test]
    fn only_an_absolute_path_to_a_folder_is_shown() {
        assert!(show_folder(Path::new("relative/folder")).is_err());
        let gone = std::env::temp_dir().join(format!("cretlink-no-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&gone);
        let error = show_folder(&gone).err();
        assert_eq!(
            error.map(|error| error.kind()),
            Some(io::ErrorKind::NotFound)
        );
    }
}
