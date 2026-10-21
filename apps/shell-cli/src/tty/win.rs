//! Windows console raw-mode control for the host terminal.
//!
//! This is the only `unsafe` code in `cretlink-shell`, deliberately localized to
//! this one platform adapter — mirroring the ConPTY exception in
//! [`cretlink_exec`]'s `conpty.rs` — while the workspace otherwise denies
//! `unsafe_code`. It flips the host's standard input into raw input (no line
//! buffering, echo, or input processing, with VT input on) and standard output
//! into VT-processing mode, and restores exactly the modes it captured on drop.
//! It carries no protocol logic and no session authority.

#![allow(
    unsafe_code,
    reason = "Localized Windows console-mode FFI (Get/SetConsoleMode, \
              GetConsoleScreenBufferInfo); the calls are confined to this \
              adapter and every acquired mode is restored on drop, mirroring \
              the crates/exec/src/conpty.rs exception to the workspace \
              `unsafe_code` deny."
)]

use std::io;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
    ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle,
    STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
};

/// A restored-on-drop guard over the host console's input and output modes.
///
/// On construction it switches stdin into raw input and stdout into VT-processing
/// mode; on drop it restores exactly the modes it captured. If enabling stdout's
/// mode fails after stdin's was changed, stdin is restored before the error is
/// returned, so a partial failure never leaves the console in a half-raw state.
#[derive(Debug)]
pub struct RawMode {
    stdin: SavedMode,
    stdout: SavedMode,
}

/// A captured console mode and the handle it belongs to.
#[derive(Debug)]
struct SavedMode {
    handle: HANDLE,
    original: CONSOLE_MODE,
}

impl RawMode {
    /// Enables raw console mode, returning a guard that restores the previous
    /// modes when dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the standard handles cannot be read or their console
    /// modes cannot be queried or set (for example, when stdin or stdout is a
    /// pipe or file rather than a console).
    pub fn enable() -> io::Result<Self> {
        let stdin_handle = std_handle(STD_INPUT_HANDLE)?;
        let stdout_handle = std_handle(STD_OUTPUT_HANDLE)?;

        let stdin_original = console_mode(stdin_handle)?;
        let stdout_original = console_mode(stdout_handle)?;

        // Raw input: drop echo, line buffering, and input processing; add VT
        // input so keystrokes stream unmodified to the remote pseudo-console.
        let stdin_raw = CONSOLE_MODE(
            (stdin_original.0
                & !(ENABLE_ECHO_INPUT.0 | ENABLE_LINE_INPUT.0 | ENABLE_PROCESSED_INPUT.0))
                | ENABLE_VIRTUAL_TERMINAL_INPUT.0,
        );
        // VT output so the remote terminal's escape sequences render locally.
        let stdout_raw = CONSOLE_MODE(
            stdout_original.0 | ENABLE_PROCESSED_OUTPUT.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0,
        );

        set_console_mode(stdin_handle, stdin_raw)?;
        if let Err(err) = set_console_mode(stdout_handle, stdout_raw) {
            // Undo the stdin change so no mode is left half-applied.
            let _ = set_console_mode(stdin_handle, stdin_original);
            return Err(err);
        }

        Ok(Self {
            stdin: SavedMode {
                handle: stdin_handle,
                original: stdin_original,
            },
            stdout: SavedMode {
                handle: stdout_handle,
                original: stdout_original,
            },
        })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = set_console_mode(self.stdin.handle, self.stdin.original);
        let _ = set_console_mode(self.stdout.handle, self.stdout.original);
    }
}

/// Best-effort host terminal size (columns, rows) from the stdout screen buffer's
/// visible window, or `None` when stdout is not a console.
#[must_use]
pub fn terminal_size() -> Option<(u16, u16)> {
    let handle = std_handle(STD_OUTPUT_HANDLE).ok()?;
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: `handle` is a live standard handle and `info` is a valid, writable
    // out-parameter for the duration of the call.
    unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }.ok()?;
    let window = info.srWindow;
    let cols = u16::try_from(i32::from(window.Right) - i32::from(window.Left) + 1).ok()?;
    let rows = u16::try_from(i32::from(window.Bottom) - i32::from(window.Top) + 1).ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    Some((cols, rows))
}

fn std_handle(which: STD_HANDLE) -> io::Result<HANDLE> {
    // SAFETY: `which` is a standard-handle selector; the call returns the
    // process's standard handle (or an error we map) and dereferences nothing.
    unsafe { GetStdHandle(which) }.map_err(io::Error::other)
}

fn console_mode(handle: HANDLE) -> io::Result<CONSOLE_MODE> {
    let mut mode = CONSOLE_MODE(0);
    // SAFETY: `handle` is a live standard handle and `mode` is a valid, writable
    // out-parameter for the duration of the call.
    unsafe { GetConsoleMode(handle, &mut mode) }.map_err(io::Error::other)?;
    Ok(mode)
}

fn set_console_mode(handle: HANDLE, mode: CONSOLE_MODE) -> io::Result<()> {
    // SAFETY: `handle` is a live standard handle; `mode` is a plain value.
    unsafe { SetConsoleMode(handle, mode) }.map_err(io::Error::other)
}
