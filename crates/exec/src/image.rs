//! The program file of this process, held and verified (ST-M009-017,
//! REQ-S-024).
//!
//! A customer can keep the support program on the computer: the elevated
//! program then writes itself to a folder that only administrators can change.
//! What it writes there must be the program that runs, and nothing else. The
//! copy that a customer downloaded runs from a folder that its account can
//! write, and a program of that account can rename the file of a running
//! program away and put another file under its path. A copy by that path
//! would then install the other file, to be started with administrator rights
//! the next time.
//!
//! So the file is not taken by the path the program was started with:
//!
//! 1. The system is asked where the image that this process is mapped from
//!    lies now. That name follows the file when it is renamed.
//! 2. The file under that name is opened for reading, shared for reading
//!    only. From then on nobody can write it, rename it or remove it, under
//!    any of its names: all of that needs an access which the open refuses to
//!    share. An open that somebody else already holds with such an access
//!    makes this one fail.
//! 3. The system is asked again where the image lies. When the answer is the
//!    name of step 1, the image lay under that name while the file under it
//!    was held: the held file is the image. When it is another name, the file
//!    was moved in between; nothing is held then, and the caller is told.
//!
//! The file of an image that is mapped cannot be written while it is mapped,
//! so the bytes read through the held file are those of the program that
//! runs.
//!
//! This rests on the file system refusing to rename a folder while a file in
//! it is held open, as NTFS does: otherwise the folder of the held file could
//! be moved away and another put in its place between the steps.
//!
//! Off Windows the customer role does not exist: nothing is held there.

use std::fs::File;
use std::io;

/// Opens the program file that this process runs from and holds it: for as
/// long as the returned file is open, nobody can write, rename or remove it.
/// The file was verified to be the image this process is mapped from (see the
/// module documentation), so what is read from it is the program that runs.
///
/// Blocking: it works on the file system.
///
/// # Errors
///
/// Off Windows, always. On Windows: when the system does not say where the
/// image lies; when the file cannot be opened without sharing changes,
/// because another program holds it in a way that allows them; or when the
/// file was moved while it was opened. Nothing is held then.
pub fn hold_running_program() -> io::Result<File> {
    sys::hold()
}

/// The steps of [`hold_running_program`], with the questions to the system
/// given: `image_name` says where the image lies now, `open` holds what
/// stands under a name, and `is_file` says whether what was held is a file.
/// What was held is given back, closed, unless the image lay under the same
/// name before and after it was held.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only Windows holds its program; the steps are tested everywhere"
    )
)]
fn hold_checked<N: PartialEq, H>(
    mut image_name: impl FnMut() -> io::Result<N>,
    open: impl FnOnce(&N) -> io::Result<H>,
    is_file: impl FnOnce(&H) -> io::Result<bool>,
) -> io::Result<H> {
    let name = image_name()?;
    let held = open(&name)?;
    if !is_file(&held)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "what stands where the program lies is not a file",
        ));
    }
    // Asked again while the file is held: see the module documentation.
    if image_name()? != name {
        return Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "the program file was moved while it was opened",
        ));
    }
    Ok(held)
}

#[cfg(not(windows))]
mod sys {
    use std::fs::File;
    use std::io;

    /// Off Windows no program keeps itself.
    pub(super) fn hold() -> io::Result<File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a program is held only on Windows",
        ))
    }
}

#[cfg(windows)]
mod sys {
    #![allow(
        unsafe_code,
        reason = "Localized Win32 calls for the program file of this process \
                  (ST-M009-017): the module of the program and the name of \
                  the file that it is mapped from, written into a buffer \
                  passed with its length. ADR-0001 permits reviewed, \
                  localized unsafe in one platform adapter."
    )]

    use std::ffi::{OsString, c_void};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;

    use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::ProcessStatus::GetMappedFileNameW;
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::core::PCWSTR;

    /// What stands before a name of the system's own name space so that the
    /// file calls of Windows take it: the root of that name space.
    const NAME_SPACE_ROOT: &str = r"\\?\GLOBALROOT";

    /// The longest name the system gives a file, in UTF-16 units.
    const MAX_NAME_UNITS: usize = 32_768;

    pub(super) fn hold() -> io::Result<File> {
        super::hold_checked(
            image_name,
            |name| open_held(name),
            |held| Ok(held.metadata()?.is_file()),
        )
    }

    /// Where the image that this process is mapped from lies now, as the
    /// system names it in its own name space
    /// (`\Device\HarddiskVolume3\…\CretLink-Support.exe`). The name is asked
    /// from the file system and follows the file when it is renamed.
    fn image_name() -> io::Result<Vec<u16>> {
        // SAFETY: without a name the call answers the module of the program
        // itself, which is loaded for as long as the process runs.
        let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(io::Error::from)?;
        let mut name = vec![0u16; MAX_NAME_UNITS];
        // SAFETY: the pseudo-handle of this process is always valid and is
        // not closed; the address is the start of the program's own image,
        // which stays mapped; `name` is a writable buffer that the wrapper
        // passes with its length.
        let len = unsafe {
            GetMappedFileNameW(
                GetCurrentProcess(),
                module.0.cast_const().cast::<c_void>(),
                &mut name,
            )
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        name.truncate(usize::try_from(len).unwrap_or(MAX_NAME_UNITS));
        Ok(name)
    }

    /// Opens the file under the system name `name` for reading, shared for
    /// reading only: while it is open nobody can write, rename or remove the
    /// file.
    fn open_held(name: &[u16]) -> io::Result<File> {
        let mut path: Vec<u16> = NAME_SPACE_ROOT.encode_utf16().collect();
        path.extend_from_slice(name);
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(PathBuf::from(OsString::from_wide(&path)))
    }
}

#[cfg(test)]
mod tests;
