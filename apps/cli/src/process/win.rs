//! Windows FFI for [`super`]: attach to the parent's console, relaunch
//! elevated through UAC, and show a message box (ST-M009-001, ST-M009-005,
//! ST-M009-006, ADR-0018).
//!
//! This, `install::win` and `shell::tty::win` are the only `unsafe` code in
//! `cretlink-cli`, each localized to its adapter. It holds no session authority
//! and carries no protocol logic.

#![allow(
    unsafe_code,
    reason = "Localized Windows FFI (AttachConsole, GetStdHandle, SetStdHandle, \
              CreateFileW, ShellExecuteExW, MessageBoxW); every pointer handed \
              to Windows borrows a value that outlives the call."
)]

use std::io;
use std::path::Path;

use windows::Win32::Foundation::{ERROR_CANCELLED, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
    STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
    MESSAGEBOX_STYLE, MessageBoxW, SW_SHOWNORMAL,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::{ConsoleAttach, Elevation};

/// Whether the standard handle refers to something (`GetStdHandle` fails for a
/// null or invalid handle).
fn has_handle(which: STD_HANDLE) -> bool {
    // SAFETY: GetStdHandle only reads the process's handle table.
    unsafe { GetStdHandle(which) }.is_ok()
}

pub(super) fn attach_parent_console() -> ConsoleAttach {
    if has_handle(STD_OUTPUT_HANDLE) {
        // Inherited from a console, or redirected (an MCP client's pipes).
        return ConsoleAttach::Inherited;
    }
    // SAFETY: AttachConsole takes a process id by value.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
        return ConsoleAttach::None;
    }
    // AttachConsole leaves missing standard handles unset in a GUI-subsystem
    // process, so point stdout and stderr at the attached console's output.
    // SAFETY: `w!` yields a static NUL-terminated wide string; the optional
    // pointers are None.
    let output: windows::core::Result<HANDLE> = unsafe {
        CreateFileW(
            w!("CONOUT$"),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    let Ok(output) = output else {
        return ConsoleAttach::None;
    };
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if !has_handle(which) {
            // SAFETY: `output` is a valid console handle that stays open for the
            // life of the process; SetStdHandle only stores it.
            let _ = unsafe { SetStdHandle(which, output) };
        }
    }
    ConsoleAttach::Attached
}

pub(super) fn relaunch_elevated(exe: &Path, parameters: &str) -> io::Result<Elevation> {
    let file = HSTRING::from(exe.as_os_str());
    let parameters = HSTRING::from(parameters);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>())
            .map_err(|_| io::Error::other("SHELLEXECUTEINFOW size overflows u32"))?,
        // Complete the launch before returning, so this process can exit.
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` and the HSTRINGs its pointers borrow live until the call
    // returns; no process handle is requested, so none needs closing.
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(Elevation::Started),
        Err(err) if err.code() == ERROR_CANCELLED.to_hresult() => Ok(Elevation::Declined),
        Err(err) => Err(io::Error::other(format!(
            "could not start {} elevated: {err}",
            exe.display()
        ))),
    }
}

pub(super) fn alert(title: &str, message: &str) {
    let _ = message_box(title, message, MB_OK | MB_ICONERROR);
}

pub(super) fn inform(title: &str, message: &str) {
    let _ = message_box(title, message, MB_OK | MB_ICONINFORMATION);
}

pub(super) fn confirm(title: &str, message: &str) -> bool {
    message_box(title, message, MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) == IDYES
}

fn message_box(
    title: &str,
    message: &str,
    style: MESSAGEBOX_STYLE,
) -> windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_RESULT {
    let title = HSTRING::from(title);
    let message = HSTRING::from(message);
    // SAFETY: both HSTRINGs outlive the modal call; no owner window is passed.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
        )
    }
}
