//! Where Windows itself lies, asked from the system (ST-M006-020, REQ-S-024).
//!
//! An elevated program must let nothing but the system decide which program it
//! starts, in which folder that program starts and where it keeps its files.
//! Three things could decide otherwise:
//!
//! - **The search for a program that is named without its folder.** Windows
//!   looks for such a program in the folder of the running program first. A
//!   downloaded copy of CretLink runs from a folder that its account can write
//!   (`Downloads`), so a `powershell.exe` or a `cmd.exe` planted there would be
//!   started with the administrator rights of the session's jobs.
//! - **The working directory**, which whoever starts a program chooses and
//!   which a job would otherwise start in. Command Prompt looks there first
//!   for a program that a command names without its folder, and Windows looks
//!   there for a library that it finds nowhere else.
//! - **The environment**, which whoever starts a program can choose:
//!   `%SystemRoot%` need not name the Windows folder.
//!
//! So the folders are asked from the system, a shell is started by its full
//! path in the system folder ([`shell_program`]), a job that names no folder
//! starts in the system folder ([`job_folder`]), and an elevated program
//! leaves the working directory it was started in
//! ([`leave_the_starters_folder`]).

use std::path::PathBuf;

/// The Windows folder as the system names it, for example `C:\Windows`.
/// `None` off Windows, and when the system does not answer.
#[must_use]
pub fn windows_folder() -> Option<PathBuf> {
    sys::windows_folder()
}

/// The system folder as the system names it, for example
/// `C:\Windows\System32`, in which Windows keeps its own programs. `None` off
/// Windows, and when the system does not answer.
#[must_use]
pub fn system_folder() -> Option<PathBuf> {
    sys::system_folder()
}

/// The folder a job starts in when its request names none: the system folder,
/// where only administrators can put anything. `None` off Windows, where a job
/// starts in the working directory of this process, and when the system does
/// not answer.
#[must_use]
pub fn job_folder() -> Option<PathBuf> {
    sys::system_folder()
}

/// The folder `spec` starts in: the one it names, otherwise [`job_folder`].
/// `None` when the job starts in the working directory of this process.
pub(crate) fn working_dir_of(spec: &crate::JobSpec) -> Option<PathBuf> {
    spec.working_dir.clone().or_else(job_folder)
}

/// Makes the system folder the working directory of this process when the
/// process is elevated. It does nothing otherwise, and nothing off Windows.
///
/// Whoever starts a program chooses its working directory. Windows looks
/// there for a library that it finds nowhere else, and a program that is
/// started without a folder of its own starts there. An elevated program can
/// have been started by an account without rights, so it leaves that folder
/// when it begins its work for a customer. A path that was given relative to
/// the old folder leads elsewhere afterwards: such a path is read before.
pub fn leave_the_starters_folder() {
    if crate::own::this_process_is_elevated()
        && let Some(folder) = sys::system_folder()
    {
        // Nothing waits for the outcome: every job is given its folder by
        // name, whatever the working directory of this process is.
        let _ = std::env::set_current_dir(folder);
    }
}

/// The program that `shell` runs as: its full path in the system folder.
///
/// # Errors
///
/// When the system does not name its folder. No shell is started then: a
/// program found by its name alone could be anyone's.
#[cfg(windows)]
pub(crate) fn shell_program(shell: crate::Shell) -> std::io::Result<PathBuf> {
    let system = sys::system_folder()
        .ok_or_else(|| std::io::Error::other("the system folder could not be read"))?;
    Ok(shell_program_in(&system, shell))
}

/// [`shell_program`] for a given system folder.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only the Windows backends start a shell by its path; the rule is tested everywhere"
    )
)]
fn shell_program_in(system: &std::path::Path, shell: crate::Shell) -> PathBuf {
    match shell {
        // Windows PowerShell, which is part of every supported Windows.
        crate::Shell::PowerShell => system
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
        crate::Shell::Cmd => system.join("cmd.exe"),
    }
}

#[cfg(not(windows))]
mod sys {
    use std::path::PathBuf;

    /// Off Windows there is no Windows folder.
    pub(super) fn windows_folder() -> Option<PathBuf> {
        None
    }

    /// Off Windows there is no system folder.
    pub(super) fn system_folder() -> Option<PathBuf> {
        None
    }
}

#[cfg(windows)]
mod sys {
    #![allow(
        unsafe_code,
        reason = "Two Win32 calls that ask the system for its own folders \
                  (ST-M006-020), each writing into a buffer passed with its \
                  length. ADR-0001 permits reviewed, localized unsafe in one \
                  platform adapter."
    )]

    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    use windows::Win32::System::SystemInformation::{
        GetSystemDirectoryW, GetSystemWindowsDirectoryW,
    };

    /// The Windows folder of the machine, whatever the session or the
    /// environment says.
    pub(super) fn windows_folder() -> Option<PathBuf> {
        // SAFETY: the buffer is writable and is passed with its length.
        read(|buffer| unsafe { GetSystemWindowsDirectoryW(Some(buffer)) })
    }

    /// The system folder, in which Windows keeps its own programs.
    pub(super) fn system_folder() -> Option<PathBuf> {
        // SAFETY: the buffer is writable and is passed with its length.
        read(|buffer| unsafe { GetSystemDirectoryW(Some(buffer)) })
    }

    /// The folder that `ask` writes into the buffer it is given, returning
    /// the length it wrote. Zero is a failure, and a length that does not fit
    /// is the size the system wanted: both are no answer.
    fn read(ask: impl FnOnce(&mut [u16]) -> u32) -> Option<PathBuf> {
        let mut buffer = [0u16; 260];
        let len = ask(&mut buffer) as usize;
        (len != 0 && len < buffer.len()).then(|| PathBuf::from(OsString::from_wide(&buffer[..len])))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::Shell;

    #[test]
    fn a_shell_is_a_program_in_the_system_folder() {
        let system = Path::new("system");
        assert_eq!(shell_program_in(system, Shell::Cmd), system.join("cmd.exe"));
        assert_eq!(
            shell_program_in(system, Shell::PowerShell),
            system
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        );
    }

    /// The system names both shells where it keeps them, and nothing in the
    /// environment moves them.
    #[cfg(windows)]
    #[test]
    fn the_shells_are_found_where_the_system_keeps_them() {
        for shell in [Shell::Cmd, Shell::PowerShell] {
            let program = shell_program(shell).unwrap_or_default();
            assert!(program.is_absolute(), "{}", program.display());
            assert!(program.is_file(), "{}", program.display());
        }
        let windows = windows_folder().unwrap_or_default();
        assert!(windows.join("explorer.exe").is_file());
        // A job that names no folder starts in the system folder.
        let system = job_folder().unwrap_or_default();
        assert!(system.is_absolute(), "{}", system.display());
        assert!(system.join("cmd.exe").is_file(), "{}", system.display());
    }

    #[cfg(not(windows))]
    #[test]
    fn there_is_no_windows_folder_off_windows() {
        assert_eq!(windows_folder(), None);
        assert_eq!(job_folder(), None);
    }

    /// A job starts in the folder its request names; a request that names
    /// none starts in the system folder, never in the working directory of
    /// this program.
    #[test]
    fn a_job_starts_in_the_folder_it_names_and_otherwise_in_the_system_folder() {
        let mut spec = crate::JobSpec::new(Shell::Cmd, "echo");
        assert_eq!(working_dir_of(&spec), job_folder());
        spec.working_dir = Some(PathBuf::from("somewhere"));
        assert_eq!(working_dir_of(&spec), Some(PathBuf::from("somewhere")));
    }
}
