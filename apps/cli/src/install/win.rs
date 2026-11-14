//! Windows FFI for [`super`] (ST-M009-006): the Start-menu shortcut, the
//! per-user uninstall entry, the process list, and removing the installed
//! folder after this process exits.
//!
//! Like `process::win`, this adapter is a localized `unsafe` exception: it
//! holds no session authority and no protocol logic, only the calls Windows
//! offers no safe API for.
//!
//! - The shortcut is a `.lnk` file, written by the shell's own `ShellLink` COM
//!   object (`IShellLinkW` + `IPersistFile`): the documented way, rather than
//!   a hand-made binary file. It runs on a thread of its own so its COM
//!   apartment never meets the window's.
//! - The uninstall entry is a key under `HKEY_CURRENT_USER`, so no
//!   administrator rights are needed.
//! - The process list (`CreateToolhelp32Snapshot` and
//!   `QueryFullProcessImageNameW`) tells whether the installed copy runs.
//!
//! The support program that a customer keeps (ST-M009-017, `crate::kept`)
//! uses the same calls for the whole computer: its entry is a key under
//! `HKEY_LOCAL_MACHINE` ([`Hive::LocalMachine`]), its shortcuts lie in the
//! Start menu and on the desktop of all users, which the system names
//! ([`common_programs`], [`public_desktop`]). The helper that removes a folder
//! after this process has exited, for either of them, is started by its full
//! path in the system folder that the system names, with an environment built
//! from the system's folders alone ([`remove_after_exit`]).

#![allow(
    unsafe_code,
    reason = "Localized Windows FFI (CoInitializeEx, CoCreateInstance, IShellLinkW, \
              IPersistFile, RegCreateKeyExW, RegSetValueExW, RegOpenKeyExW, \
              RegDeleteTreeW, RegGetValueW, CreateToolhelp32Snapshot, \
              Process32FirstW/NextW, OpenProcess, QueryFullProcessImageNameW, \
              SHGetKnownFolderPath, CoTaskMemFree, GetFileInformationByHandle, \
              IShellLinkW::GetPath, IPersistFile::Load); every pointer handed to Windows borrows a value \
              that outlives the call, and every handle is closed by an owner."
)]

use std::ffi::{OsString, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, WIN32_ERROR};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle, WIN32_FIND_DATAW,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile, STGM_READ,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteTreeW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_PublicDesktop, IShellLinkW, KF_FLAG_DEFAULT,
    SHGetKnownFolderPath, ShellLink,
};
use windows::core::{GUID, HSTRING, Interface, PCWSTR, PWSTR};

use super::{Shortcut, UNINSTALL_KEY, UninstallEntry};

/// `CREATE_NO_WINDOW`: the removal helper opens no console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn check(status: WIN32_ERROR) -> io::Result<()> {
    status.ok().map_err(io::Error::from)
}

/// Closes a registry key when dropped.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by this module and is closed once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// Closes a kernel handle when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn set_value(key: &Key, name: &str, kind: REG_VALUE_TYPE, data: &[u8]) -> io::Result<()> {
    let name = HSTRING::from(name);
    // SAFETY: `name` and `data` outlive the call; the key is open for writing.
    check(unsafe { RegSetValueExW(key.0, &name, None, kind, Some(data)) })
}

fn set_string(key: &Key, name: &str, value: &str) -> io::Result<()> {
    // REG_SZ data is the UTF-16 string with its terminating NUL, in bytes.
    let data: Vec<u8> = value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    set_value(key, name, REG_SZ, &data)
}

/// The part of the registry an uninstall entry lies in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hive {
    /// For this user: the technician's install, which needs no administrator
    /// rights.
    CurrentUser,
    /// For the whole computer: the support program that a customer keeps,
    /// which only administrators can change.
    LocalMachine,
}

impl Hive {
    fn root(self) -> HKEY {
        match self {
            Self::CurrentUser => HKEY_CURRENT_USER,
            Self::LocalMachine => HKEY_LOCAL_MACHINE,
        }
    }
}

pub(super) fn write_uninstall_entry(entry: &UninstallEntry) -> io::Result<()> {
    write_uninstall_entry_in(Hive::CurrentUser, UNINSTALL_KEY, entry)
}

/// Writes the uninstall entry under `key` of `hive`.
pub(crate) fn write_uninstall_entry_in(
    hive: Hive,
    key: &str,
    entry: &UninstallEntry,
) -> io::Result<()> {
    let path = HSTRING::from(key);
    let mut key = HKEY::default();
    // SAFETY: `path` outlives the call and `key` receives the opened key, which
    // `Key` closes.
    check(unsafe {
        RegCreateKeyExW(
            hive.root(),
            &path,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    })?;
    let key = Key(key);
    for (name, value) in [
        ("DisplayName", &entry.display_name),
        ("DisplayVersion", &entry.display_version),
        ("Publisher", &entry.publisher),
        ("InstallLocation", &entry.install_location),
        ("UninstallString", &entry.uninstall_string),
        ("DisplayIcon", &entry.display_icon),
    ] {
        set_string(&key, name, value)?;
    }
    // Uninstall is the only action the entry offers.
    for name in ["NoModify", "NoRepair"] {
        set_value(&key, name, REG_DWORD, &1u32.to_le_bytes())?;
    }
    Ok(())
}

pub(super) fn has_uninstall_entry() -> bool {
    has_uninstall_entry_in(Hive::CurrentUser, UNINSTALL_KEY)
}

/// Whether the uninstall entry under `key` of `hive` exists.
pub(crate) fn has_uninstall_entry_in(hive: Hive, key: &str) -> bool {
    let path = HSTRING::from(key);
    let mut key = HKEY::default();
    // SAFETY: as in `write_uninstall_entry_in`; read access only.
    let status = unsafe { RegOpenKeyExW(hive.root(), &path, None, KEY_READ, &mut key) };
    if status.is_ok() {
        drop(Key(key));
        true
    } else {
        false
    }
}

/// The location that the uninstall entry under `key` of `hive` names
/// (`InstallLocation`). `None` when there is no entry, or it names none.
pub(crate) fn uninstall_entry_location_in(hive: Hive, key: &str) -> Option<PathBuf> {
    let path = HSTRING::from(key);
    let value = HSTRING::from("InstallLocation");
    let mut buffer = vec![0u16; 32_768];
    let mut len = u32::try_from(buffer.len() * 2).ok()?;
    // SAFETY: `path` and `value` outlive the call; `buffer` is writable and
    // holds `len` bytes, and the call writes the length it used into `len`.
    let status = unsafe {
        RegGetValueW(
            hive.root(),
            &path,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            Some(&mut len),
        )
    };
    check(status).ok()?;
    // The length is in bytes and counts the terminating NUL.
    buffer.truncate(usize::try_from(len).ok()? / 2);
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    (!buffer.is_empty()).then(|| PathBuf::from(OsString::from_wide(&buffer)))
}

/// How many names (hard links) the open `file` has.
pub(crate) fn number_of_links(file: &File) -> io::Result<u32> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle belongs to `file`, which outlives the call; `info`
    // is a valid place for the answer.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(io::Error::from)?;
    Ok(info.nNumberOfLinks)
}

pub(super) fn remove_uninstall_entry() -> io::Result<bool> {
    remove_uninstall_entry_in(Hive::CurrentUser, UNINSTALL_KEY)
}

/// Removes the uninstall entry under `key` of `hive`; `Ok(false)` when there
/// was none.
pub(crate) fn remove_uninstall_entry_in(hive: Hive, key: &str) -> io::Result<bool> {
    let path = HSTRING::from(key);
    // SAFETY: `path` outlives the call.
    let status = unsafe { RegDeleteTreeW(hive.root(), &path) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    check(status).map(|()| true)
}

/// The Start-menu folder of all users, as the system names it.
pub(crate) fn common_programs() -> Option<PathBuf> {
    known_folder(&FOLDERID_CommonPrograms)
}

/// The desktop folder of all users, as the system names it.
pub(crate) fn public_desktop() -> Option<PathBuf> {
    known_folder(&FOLDERID_PublicDesktop)
}

/// A folder that the system knows by `id`. `None` when it does not name one.
fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: `id` outlives the call; without a token the folder of the
    // calling account is asked, which for a folder of all users is the same.
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: on success `path` is a NUL-terminated wide string that stays
    // valid until it is freed below.
    let folder = PathBuf::from(OsString::from_wide(unsafe { path.as_wide() }));
    // SAFETY: the string was allocated by the system for the caller to free
    // with `CoTaskMemFree`, and is not used afterwards.
    unsafe { CoTaskMemFree(Some(path.0.cast_const().cast::<c_void>())) };
    Some(folder)
}

pub(crate) fn create_shortcut(shortcut: &Shortcut) -> io::Result<()> {
    if let Some(parent) = shortcut.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let shortcut = shortcut.clone();
    std::thread::spawn(move || write_shortcut(&shortcut))
        .join()
        .map_err(|_| io::Error::other("the shortcut could not be written"))?
}

/// Writes the `.lnk` on the calling thread, which must be a fresh one.
fn write_shortcut(shortcut: &Shortcut) -> io::Result<()> {
    // SAFETY: a fresh thread, initialised once and uninitialised below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(io::Error::from)?;
    let result = (|| -> windows::core::Result<()> {
        let target = HSTRING::from(shortcut.target.as_os_str());
        let arguments = HSTRING::from(shortcut.arguments.as_str());
        let description = HSTRING::from(shortcut.description.as_str());
        let path = HSTRING::from(shortcut.path.as_os_str());
        // SAFETY: COM is initialised on this thread; every HSTRING outlives
        // the call it is passed to; the interfaces are released on drop.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&target)?;
            link.SetArguments(&arguments)?;
            link.SetDescription(&description)?;
            let file: IPersistFile = link.cast()?;
            file.Save(&path, true)
        }
    })();
    // SAFETY: balances the successful CoInitializeEx above, after every
    // interface of this thread has been released.
    unsafe { CoUninitialize() };
    result.map_err(io::Error::from)
}

/// The program that the shortcut at `path` starts, as written in it: read by
/// the shell's own `ShellLink` object on a thread of its own, without
/// resolving the shortcut and without expanding anything in it.
pub(crate) fn shortcut_target(path: &Path) -> io::Result<PathBuf> {
    let path = path.to_path_buf();
    std::thread::spawn(move || read_shortcut(&path))
        .join()
        .map_err(|_| io::Error::other("the shortcut could not be read"))?
}

/// Reads the target of the `.lnk` on the calling thread, which must be a
/// fresh one.
fn read_shortcut(path: &Path) -> io::Result<PathBuf> {
    /// `SLGP_RAWPATH`: the path as it was written, nothing expanded.
    const SLGP_RAWPATH: u32 = 0x4;
    // SAFETY: a fresh thread, initialised once and uninitialised below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(io::Error::from)?;
    let result = (|| -> windows::core::Result<Vec<u16>> {
        let path = HSTRING::from(path.as_os_str());
        let mut target = vec![0u16; 32_768];
        let mut found = WIN32_FIND_DATAW::default();
        // SAFETY: COM is initialised on this thread; `path` outlives the
        // call it is passed to; `target` and `found` are writable and
        // outlive the call; the interfaces are released on drop.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            let file: IPersistFile = link.cast()?;
            file.Load(&path, STGM_READ)?;
            link.GetPath(&mut target, &raw mut found, SLGP_RAWPATH)?;
        }
        Ok(target)
    })();
    // SAFETY: balances the successful CoInitializeEx above, after every
    // interface of this thread has been released.
    unsafe { CoUninitialize() };
    let target = result.map_err(io::Error::from)?;
    let len = target
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(target.len());
    Ok(PathBuf::from(OsString::from_wide(&target[..len])))
}

/// The full image path of a process, when this user may query it.
fn image_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: OpenProcess takes plain values; the handle is closed by `Handle`.
    let handle =
        Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut buffer = vec![0u16; 32_768];
    let mut len = u32::try_from(buffer.len()).ok()?;
    // SAFETY: `buffer` holds `len` UTF-16 units and outlives the call, which
    // writes the path's length back into `len`.
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    buffer.truncate(usize::try_from(len).ok()?);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

pub(crate) fn running_elsewhere(exe: &Path) -> io::Result<bool> {
    let me = std::process::id();
    let Some(wanted) = exe
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
    else {
        return Ok(false);
    };
    // SAFETY: plain values in; the snapshot handle is closed by `Handle`.
    let snapshot = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?);
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>())
            .map_err(|_| io::Error::other("PROCESSENTRY32W size overflows u32"))?,
        ..Default::default()
    };
    // SAFETY: `entry` is a correctly sized PROCESSENTRY32W that outlives each
    // call; the snapshot is open.
    let mut next = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while next.is_ok() {
        let pid = entry.th32ProcessID;
        let len = entry
            .szExeFile
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
        // Only a process with the same file name is worth asking for its path;
        // one this user may not query is not this user's copy.
        if pid != me
            && name == wanted
            && image_path(pid).is_some_and(|path| super::same_path(&path, exe))
        {
            return Ok(true);
        }
        // SAFETY: as above.
        next = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    Ok(false)
}

/// Starts the removal helper, detached and without a window: it waits for
/// this process to exit, then removes `files` and `dir` if it is empty.
///
/// A running Windows program cannot delete its own file, and `cmd.exe` cannot
/// wait for a process id, so the helper is the in-box Windows PowerShell
/// (`Wait-Process`). It is started as [`super::removal_helper`] says: by its
/// full path in the system folder, which the system names, in that folder,
/// with an environment built from the system's folders alone and nothing of
/// the environment of this process (ST-M009-017, REQ-S-024). The script uses
/// cmdlets only, so it also runs under Constrained Language mode, and the
/// execution policy does not apply to `-Command`.
pub(crate) fn remove_after_exit(files: &[PathBuf], dir: &Path) -> io::Result<()> {
    let windows = cretlink_exec::windows_folder()
        .ok_or_else(|| io::Error::other("the Windows folder could not be read"))?;
    let system = cretlink_exec::system_folder()
        .ok_or_else(|| io::Error::other("the system folder could not be read"))?;
    let helper = super::removal_helper(&windows, &system, std::process::id(), files, dir);
    Command::new(&helper.program)
        .args(&helper.args)
        // Its whole environment is the helper's own: nothing is passed on.
        .env_clear()
        .envs(helper.env.iter().map(|(name, value)| (name, value)))
        // Not inside the folder it removes.
        .current_dir(&helper.current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(drop)
}
