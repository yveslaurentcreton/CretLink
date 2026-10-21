//! Windows desktop capture-and-input backend (ADR-0015, ST-M005-001/002/003).
//!
//! This is the only place the desktop feature touches the operating system, and
//! the only `unsafe` in the desktop module — deliberately localized to one
//! Windows adapter exactly as [`crate::conpty`] and the piped-mode
//! [`crate::platform`] Job Object are, per ADR-0001/ADR-0006. Everything above
//! it ([`super`]) is portable, pure, and unit-tested on every platform.
//!
//! # What it does
//!
//! - **Capture** takes a GDI snapshot of the full virtual desktop with
//!   `StretchBlt` (which both copies 1:1 and downscales to honour the caller's
//!   `max_width`/`max_height` hints), converts BGRA to RGBA, and hands the pixels
//!   to the pure [`super::encode_png`]. Per-display selection and DXGI Desktop
//!   Duplication are documented follow-ups; this backbone captures display 0 (the
//!   whole virtual desktop) and refuses other indices honestly.
//! - **Honest state** is probed *before* any capture: a locked, switched,
//!   disconnected, secure, or no-input-desktop session returns explicit metadata
//!   with **no** image, never a fabricated or stale frame (REQ-N-008).
//! - **Input** (pointer, scroll, key, type, hotkey) is synthesized with
//!   `SendInput` using absolute virtual-desktop coordinates.
//! - **Clipboard** get/set exchange `CF_UNICODETEXT` through the Win32 clipboard.
//!
//! # Honesty and secrecy
//!
//! Every OS failure becomes an [`ExecError::DesktopBackend`] whose message names
//! the failure without echoing any captured pixels, clipboard text, or
//! synthesized keystrokes (REQ-N-015). No frame or input is ever silently
//! dropped or fabricated.

#![allow(
    unsafe_code,
    reason = "Localized Windows GDI capture / SendInput / clipboard / \
              window-station FFI for the interactive-desktop backend \
              (ADR-0015); ADR-0001 permits reviewed, localized unsafe in one \
              platform adapter."
)]

use std::ffi::c_void;

use cretlink_protocol::{
    DesktopFrameMeta, DesktopState, ImageFormat, KeyAction, PointerAction, PointerButton, limits,
};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GdiFlush, GetDC, HALFTONE, HDC, HGDIOBJ, ReleaseDC, SRCCOPY,
    SelectObject, SetStretchBltMode, StretchBlt,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetThreadDesktop,
    GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::{
    CaptureRequest, CapturedFrame, bgra_to_rgba, encode_png, key_name_to_vk, normalize_virtual,
};
use crate::error::ExecError;

/// `CF_UNICODETEXT`, the UTF-16 clipboard format. Spelled as a literal to keep
/// the clipboard calls free of a format-enum feature guess.
const CF_UNICODETEXT: u32 = 13;

/// One wheel detent, as `SendInput` expects in `MOUSEINPUT::mouseData`.
const WHEEL_DELTA: i32 = 120;

/// Builds a content-free backend error naming the failure, with no OS payload.
fn os_err(context: &'static str) -> ExecError {
    ExecError::DesktopBackend(std::io::Error::other(context))
}

/// Maps a `windows` FFI error into a content-free backend error (its HRESULT,
/// never any captured content).
fn win_err(context: &'static str, err: &windows::core::Error) -> ExecError {
    ExecError::DesktopBackend(std::io::Error::other(format!(
        "{context}: 0x{:08X}",
        err.code().0 as u32
    )))
}

// --- Capture ---

pub(super) fn capture(request: &CaptureRequest) -> Result<CapturedFrame, ExecError> {
    let state = detect_state();
    let wts_session = active_session();
    let desktop = current_desktop_path();
    let dpi = system_dpi();

    // Honest state: only an Active desktop carries an image. Every other state
    // returns explicit metadata with no pixels — never a stale or blank frame
    // dressed up as live (REQ-N-008).
    if state != DesktopState::Active {
        return Ok(CapturedFrame {
            meta: DesktopFrameMeta {
                state,
                wts_session,
                desktop,
                display: request.display,
                width: 0,
                height: 0,
                physical_width: 0,
                physical_height: 0,
                dpi,
                format: ImageFormat::Png,
                captured_unix_ms: now_unix_ms(),
            },
            image: None,
        });
    }

    // This backbone captures display 0 = the whole virtual desktop. Per-display
    // selection is a documented follow-up; an out-of-range index is refused
    // honestly rather than silently returning the wrong screen.
    if request.display != 0 {
        return Err(ExecError::DesktopBackend(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "only display 0 (the full virtual desktop) is captured in this build",
        )));
    }

    let origin_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let origin_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let src_w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let src_h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if src_w <= 0 || src_h <= 0 {
        return Err(os_err("virtual desktop has no positive extent"));
    }

    let (enc_w, enc_h) = fit_dimensions(
        src_w as u32,
        src_h as u32,
        request.max_width,
        request.max_height,
    );

    let rgba = grab_virtual(origin_x, origin_y, src_w, src_h, enc_w, enc_h)?;
    let png = encode_png(&rgba, enc_w, enc_h)?;

    // Logical (DPI-scaled) size is derived from the encoded pixel size so the
    // console can render at the right CSS size regardless of downscaling.
    let width = scale_logical(enc_w, dpi);
    let height = scale_logical(enc_h, dpi);

    Ok(CapturedFrame {
        meta: DesktopFrameMeta {
            state: DesktopState::Active,
            wts_session,
            desktop,
            display: 0,
            width,
            height,
            physical_width: enc_w,
            physical_height: enc_h,
            dpi,
            format: ImageFormat::Png,
            captured_unix_ms: now_unix_ms(),
        },
        image: Some(png),
    })
}

/// Downscales `(src_w, src_h)` to fit within the caller's longest-edge hints,
/// preserving aspect ratio. With no hints the native size is kept. The result is
/// never upscaled and never smaller than 1x1.
fn fit_dimensions(src_w: u32, src_h: u32, max_w: Option<u16>, max_h: Option<u16>) -> (u32, u32) {
    let mut scale = 1.0_f64;
    if let Some(mw) = max_w
        && src_w > u32::from(mw)
    {
        scale = scale.min(f64::from(mw) / f64::from(src_w));
    }
    if let Some(mh) = max_h
        && src_h > u32::from(mh)
    {
        scale = scale.min(f64::from(mh) / f64::from(src_h));
    }
    if scale >= 1.0 {
        return (src_w.max(1), src_h.max(1));
    }
    let w = ((f64::from(src_w) * scale).round() as u32).max(1);
    let h = ((f64::from(src_h) * scale).round() as u32).max(1);
    (w, h)
}

/// Scales an encoded pixel dimension to a logical (96-DPI) dimension.
fn scale_logical(px: u32, dpi: u16) -> u32 {
    let dpi = u32::from(dpi.max(1));
    px.saturating_mul(96) / dpi
}

/// Grabs the virtual-desktop region into a tightly packed top-down RGBA buffer,
/// scaling to `enc_w`x`enc_h` with `StretchBlt`. The screen DC is released on
/// every path.
fn grab_virtual(
    origin_x: i32,
    origin_y: i32,
    src_w: i32,
    src_h: i32,
    enc_w: u32,
    enc_h: u32,
) -> Result<Vec<u8>, ExecError> {
    // SAFETY: `GetDC(None)` returns the screen DC, released once below on both
    // the success and error paths.
    let screen = unsafe { GetDC(None) };
    if screen.0.is_null() {
        return Err(os_err("GetDC(NULL) failed"));
    }
    let result = grab_into(screen, origin_x, origin_y, src_w, src_h, enc_w, enc_h);
    unsafe { ReleaseDC(None, screen) };
    result
}

/// The inner GDI sequence, with `screen` guaranteed live and released by the
/// caller. Its own transient objects (memory DC, DIB section) are released here.
fn grab_into(
    screen: HDC,
    origin_x: i32,
    origin_y: i32,
    src_w: i32,
    src_h: i32,
    enc_w: u32,
    enc_h: u32,
) -> Result<Vec<u8>, ExecError> {
    let mem = unsafe { CreateCompatibleDC(Some(screen)) };
    if mem.0.is_null() {
        return Err(os_err("CreateCompatibleDC failed"));
    }

    // A top-down (negative height) 32-bit BI_RGB DIB gives direct BGRA pixel
    // access with no bottom-up row flip.
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
            biWidth: enc_w as i32,
            biHeight: -(enc_h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = unsafe {
        CreateDIBSection(
            Some(screen),
            std::ptr::from_ref(&info),
            DIB_RGB_COLORS,
            std::ptr::from_mut(&mut bits),
            None,
            0,
        )
    };
    let dib = match dib {
        Ok(h) if !h.0.is_null() && !bits.is_null() => h,
        Ok(h) => {
            if !h.0.is_null() {
                let _ = unsafe { DeleteObject(HGDIOBJ(h.0)) };
            }
            let _ = unsafe { DeleteDC(mem) };
            return Err(os_err("CreateDIBSection returned no pixel buffer"));
        }
        Err(e) => {
            let _ = unsafe { DeleteDC(mem) };
            return Err(win_err("CreateDIBSection failed", &e));
        }
    };

    let old = unsafe { SelectObject(mem, HGDIOBJ(dib.0)) };
    let _ = unsafe { SetStretchBltMode(mem, HALFTONE) };
    let blt = unsafe {
        StretchBlt(
            mem,
            0,
            0,
            enc_w as i32,
            enc_h as i32,
            Some(screen),
            origin_x,
            origin_y,
            src_w,
            src_h,
            SRCCOPY,
        )
    };
    let _ = unsafe { GdiFlush() };

    let result = if blt.as_bool() {
        let len = (enc_w as usize) * (enc_h as usize) * 4;
        // SAFETY: `bits` points at a DIB of exactly `enc_w * enc_h` 32-bit
        // pixels, top-down, so a tightly packed `len`-byte read is in bounds.
        let bgra = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), len) };
        Ok(bgra_to_rgba(bgra, enc_w, enc_h, enc_w as usize * 4))
    } else {
        Err(os_err("StretchBlt failed"))
    };

    unsafe { SelectObject(mem, old) };
    let _ = unsafe { DeleteObject(HGDIOBJ(dib.0)) };
    let _ = unsafe { DeleteDC(mem) };
    result
}

// --- Honest desktop state ---

/// Probes the live desktop state without ever fabricating a frame. Best-effort
/// and conservative: when the input desktop cannot be opened the session is
/// reported [`DesktopState::Locked`] (the most common cause), and the services
/// session is reported [`DesktopState::NoInputDesktop`]. The maintainer refines
/// the exact transitions on real locked/switched/secure hardware (ST-M005-001).
fn detect_state() -> DesktopState {
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    if console == 0xFFFF_FFFF {
        return DesktopState::Disconnected;
    }
    let mut ours: u32 = 0;
    let have_ours =
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), std::ptr::from_mut(&mut ours)) }
            .is_ok();
    if have_ours && ours == 0 {
        // Session 0 is the non-interactive services session: no visible desktop.
        return DesktopState::NoInputDesktop;
    }
    if have_ours && ours != console {
        // A different session holds the console (fast user switch).
        return DesktopState::Switched;
    }
    match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) } {
        Ok(hdesk) => {
            let name = object_name(HANDLE(hdesk.0));
            let _ = unsafe { CloseDesktop(hdesk) };
            match name.as_deref() {
                Some("Winlogon") | Some("Screen-saver") => DesktopState::Secure,
                _ => DesktopState::Active,
            }
        }
        // The input desktop is not reachable: the workstation is locked, or a
        // secure desktop is up. Locked is the conservative, common answer; we
        // never capture in this state regardless.
        Err(_) => DesktopState::Locked,
    }
}

/// The active console (WTS) session id, falling back to this process's session.
fn active_session() -> u32 {
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    if console != 0xFFFF_FFFF {
        return console;
    }
    let mut ours: u32 = 0;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), std::ptr::from_mut(&mut ours)) }.is_ok()
    {
        ours
    } else {
        0
    }
}

/// The `WinSta0\<desktop>` path this thread runs on, best-effort. The station is
/// reported as the interactive `WinSta0`; the desktop name is read from the
/// thread's desktop. Truncated to the protocol's name bound.
fn current_desktop_path() -> String {
    let name = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
        .ok()
        .and_then(|hdesk| object_name(HANDLE(hdesk.0)))
        .unwrap_or_else(|| "Default".to_string());
    let mut path = format!("WinSta0\\{name}");
    if path.len() > limits::MAX_DESKTOP_NAME_BYTES {
        path.truncate(limits::MAX_DESKTOP_NAME_BYTES);
    }
    path
}

/// Reads a user object's `UOI_NAME` (a window-station or desktop handle), or
/// `None` when it cannot be read.
fn object_name(handle: HANDLE) -> Option<String> {
    let mut needed: u32 = 0;
    // First call: learn the required buffer length.
    let _ = unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            None,
            0,
            Some(std::ptr::from_mut(&mut needed)),
        )
    };
    if needed == 0 || needed > 1024 {
        return None;
    }
    let mut buf = vec![0u8; needed as usize];
    unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            needed,
            None,
        )
    }
    .ok()?;
    // The buffer is a NUL-terminated wide string.
    let wide: &[u16] =
        unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u16>(), (needed as usize) / 2) };
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    Some(String::from_utf16_lossy(&wide[..end]))
}

/// The system DPI, clamped into `u16`.
fn system_dpi() -> u16 {
    let dpi = unsafe { GetDpiForSystem() };
    u16::try_from(dpi).unwrap_or(96)
}

/// The current Unix time in milliseconds, or 0 if the clock is before the epoch.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// --- Input synthesis ---

pub(super) fn pointer(
    action: PointerAction,
    button: PointerButton,
    x: i32,
    y: i32,
) -> Result<(), ExecError> {
    let (nx, ny) = normalize_point(x, y);
    let mut inputs: Vec<INPUT> = Vec::with_capacity(4);
    // A move always precedes a button event so the click lands at (x, y).
    inputs.push(mouse_input(nx, ny, 0, MOUSEEVENTF_MOVE));
    let (down, up) = button_flags(button);
    match action {
        PointerAction::Move => {}
        PointerAction::Down => inputs.push(mouse_input(nx, ny, 0, down)),
        PointerAction::Up => inputs.push(mouse_input(nx, ny, 0, up)),
        PointerAction::Click => {
            inputs.push(mouse_input(nx, ny, 0, down));
            inputs.push(mouse_input(nx, ny, 0, up));
        }
        PointerAction::DoubleClick => {
            inputs.push(mouse_input(nx, ny, 0, down));
            inputs.push(mouse_input(nx, ny, 0, up));
            inputs.push(mouse_input(nx, ny, 0, down));
            inputs.push(mouse_input(nx, ny, 0, up));
        }
    }
    send(&inputs)
}

pub(super) fn scroll(x: i32, y: i32, dx: i32, dy: i32) -> Result<(), ExecError> {
    let (nx, ny) = normalize_point(x, y);
    let mut inputs: Vec<INPUT> = Vec::with_capacity(3);
    inputs.push(mouse_input(nx, ny, 0, MOUSEEVENTF_MOVE));
    if dy != 0 {
        inputs.push(mouse_input(nx, ny, dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL));
    }
    if dx != 0 {
        inputs.push(mouse_input(nx, ny, dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL));
    }
    if inputs.len() == 1 {
        // No wheel motion requested; nothing to do beyond the (harmless) move.
        return Ok(());
    }
    send(&inputs)
}

pub(super) fn key(action: KeyAction, key: &str) -> Result<(), ExecError> {
    let vk = key_name_to_vk(key).ok_or_else(|| {
        ExecError::DesktopBackend(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unknown key name",
        ))
    })?;
    let mut inputs: Vec<INPUT> = Vec::with_capacity(2);
    match action {
        KeyAction::Down => inputs.push(key_input(vk, false)),
        KeyAction::Up => inputs.push(key_input(vk, true)),
        KeyAction::Press => {
            inputs.push(key_input(vk, false));
            inputs.push(key_input(vk, true));
        }
    }
    send(&inputs)
}

pub(super) fn type_text(text: &str) -> Result<(), ExecError> {
    let mut inputs: Vec<INPUT> = Vec::new();
    for unit in text.encode_utf16() {
        inputs.push(unicode_input(unit, false));
        inputs.push(unicode_input(unit, true));
    }
    if inputs.is_empty() {
        return Ok(());
    }
    send(&inputs)
}

pub(super) fn hotkey(keys: &[String]) -> Result<(), ExecError> {
    let mut vks: Vec<u16> = Vec::with_capacity(keys.len());
    for name in keys {
        let vk = key_name_to_vk(name).ok_or_else(|| {
            ExecError::DesktopBackend(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unknown key name in hotkey chord",
            ))
        })?;
        vks.push(vk);
    }
    if vks.is_empty() {
        return Ok(());
    }
    let mut inputs: Vec<INPUT> = Vec::with_capacity(vks.len() * 2);
    // Press in order, release in reverse — the usual chord semantics.
    for &vk in &vks {
        inputs.push(key_input(vk, false));
    }
    for &vk in vks.iter().rev() {
        inputs.push(key_input(vk, true));
    }
    send(&inputs)
}

/// Clamps a virtual-desktop pixel coordinate into the normalized absolute range.
fn normalize_point(x: i32, y: i32) -> (i32, i32) {
    let origin_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let origin_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let span_x = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let span_y = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    (
        normalize_virtual(x, origin_x, span_x),
        normalize_virtual(y, origin_y, span_y),
    )
}

fn button_flags(button: PointerButton) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
    match button {
        PointerButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        PointerButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        PointerButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

fn mouse_input(nx: i32, ny: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: nx,
                dy: ny,
                mouseData: data as u32,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(vk: u16, up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_input(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Submits a batch of synthesized inputs in one `SendInput` call. A short count
/// means the OS blocked or dropped input (for example the secure desktop is up).
fn send(inputs: &[INPUT]) -> Result<(), ExecError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(os_err("SendInput did not deliver all events"))
    }
}

// --- Clipboard ---

pub(super) fn clipboard_get() -> Result<String, ExecError> {
    unsafe { OpenClipboard(None) }.map_err(|e| win_err("OpenClipboard failed", &e))?;
    // Read within a helper so the clipboard is always closed afterwards.
    let text = clipboard_read_unicode();
    let _ = unsafe { CloseClipboard() };
    text
}

fn clipboard_read_unicode() -> Result<String, ExecError> {
    let handle = match unsafe { GetClipboardData(CF_UNICODETEXT) } {
        Ok(h) if !h.is_invalid() => h,
        // No Unicode text on the clipboard is an empty string, not an error.
        _ => return Ok(String::new()),
    };
    let hglobal = HGLOBAL(handle.0);
    let ptr = unsafe { GlobalLock(hglobal) } as *const u16;
    if ptr.is_null() {
        return Ok(String::new());
    }
    // Read up to the clipboard cap of UTF-16 units, stopping at the NUL.
    let cap = limits::MAX_CLIPBOARD_BYTES / 2;
    let mut len = 0usize;
    while len < cap && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf16_lossy(slice);
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(text)
}

pub(super) fn clipboard_set(text: &str) -> Result<(), ExecError> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = utf16.len() * 2;

    unsafe { OpenClipboard(None) }.map_err(|e| win_err("OpenClipboard failed", &e))?;
    let result = clipboard_write_unicode(&utf16, bytes);
    let _ = unsafe { CloseClipboard() };
    result
}

fn clipboard_write_unicode(utf16: &[u16], bytes: usize) -> Result<(), ExecError> {
    unsafe { EmptyClipboard() }.map_err(|e| win_err("EmptyClipboard failed", &e))?;
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
        .map_err(|e| win_err("GlobalAlloc failed", &e))?;
    let dst = unsafe { GlobalLock(hglobal) } as *mut u16;
    if dst.is_null() {
        let _ = unsafe { GlobalFree(Some(hglobal)) };
        return Err(os_err("GlobalLock failed"));
    }
    unsafe { std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len()) };
    let _ = unsafe { GlobalUnlock(hglobal) };
    // On success the system owns the memory; on failure we must free it.
    match unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hglobal.0))) } {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = unsafe { GlobalFree(Some(hglobal)) };
            Err(win_err("SetClipboardData failed", &e))
        }
    }
}
