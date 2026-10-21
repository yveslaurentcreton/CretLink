//! Interactive-desktop capture and input backend (ADR-0015,
//! ST-M005-001/002/003).
//!
//! This is the agent-side seam the session runtime binds behind the
//! `desktop.v1` capability gate. It reuses the protocol vocabulary
//! ([`cretlink_protocol::DesktopState`], [`ImageFormat`], [`PointerAction`], [`PointerButton`],
//! [`KeyAction`], [`DesktopFrameMeta`]) so the session forwards a captured frame
//! without a lossy conversion, exactly as the broker reuses [`crate::JobOutcome`].
//!
//! # Layering
//!
//! The pure, cross-platform helpers ([`encode_png`], [`bgra_to_rgba`],
//! [`normalize_virtual`], [`key_name_to_vk`]) carry the frame-encoding and
//! input-mapping logic and are unit-tested on every platform in the matrix. The
//! operating-system calls that actually capture pixels and synthesize input are
//! confined to the `#[cfg(windows)]` [`platform`] module — mirroring how
//! [`crate::conpty`] confines the pseudo-console FFI. Off Windows the backend is
//! an honest stub that fails closed with [`ExecError::DesktopUnsupported`].
//!
//! Because [`Desktop::backend_available`] reports the truth per platform, a
//! production agent advertises `desktop.v1` only where a real backend exists
//! (REQ-S-009, REQ-N-012); a peer never sees the capability on a platform that
//! would answer every desktop operation with [`ExecError::DesktopUnsupported`].
//!
//! # Honest state, never a fabricated frame
//!
//! Capture reports the live [`cretlink_protocol::DesktopState`] and carries an
//! image only for [`cretlink_protocol::DesktopState::Active`]. A locked,
//! switched, disconnected, missing, or secure desktop returns explicit metadata
//! with **no** image — never a stale or blank frame passed off as live
//! (REQ-N-008).

use std::time::{Duration, Instant};

use cretlink_protocol::{
    DesktopFrameMeta, ImageFormat, KeyAction, PointerAction, PointerButton, limits,
};

use crate::error::ExecError;

/// A desktop-capture request, carrying the display selector, the requested
/// encoding, and the optional downscale and quality hints already validated by
/// the protocol layer (`cretlink_protocol::limits`).
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    /// The zero-based display index to capture.
    pub display: u16,
    /// The image encoding the console asked for.
    pub format: ImageFormat,
    /// An optional longest-edge width hint; `None` means capture at native size.
    pub max_width: Option<u16>,
    /// An optional longest-edge height hint; `None` means capture at native size.
    pub max_height: Option<u16>,
    /// An optional encoder quality on a 1..=100 scale; `None` uses the backend
    /// default for the chosen [`ImageFormat`].
    pub quality: Option<u8>,
}

/// One captured desktop frame: its honest [`DesktopFrameMeta`] identity plus the
/// encoded image bytes, which are present only when
/// [`DesktopFrameMeta::state`] is [`cretlink_protocol::DesktopState::Active`]. The session layer
/// decides whether to inline the image or stage it (ADR-0015); the backend never
/// fabricates an image for a locked, switched, disconnected, or secure desktop.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// The frame's self-describing identity and live desktop state.
    pub meta: DesktopFrameMeta,
    /// The encoded image bytes, or `None` for any non-[`cretlink_protocol::DesktopState::Active`]
    /// state.
    pub image: Option<Vec<u8>>,
}

// --- Capture backpressure (REQ-N-006, ST-M005-001) ---

/// The documented desktop-capture backpressure bounds (REQ-N-006).
///
/// The screenshot model is on-demand / paced-refresh (ADR-0015), not a video
/// codec, so the bound that actively bites under load is the refresh **rate**:
/// captures requested faster than [`Self::min_interval`] are paced down to it.
/// The in-flight and memory guards bound the agent's peak capture memory; they
/// hold by construction while the agent serves one capture at a time and hands
/// each frame straight to the transport, and become the load-bearing runtime
/// guard once staged-frame retention (ADR-0015) lets more than one frame be held
/// at once.
#[derive(Debug, Clone, Copy)]
pub struct CaptureLimits {
    /// The minimum spacing between two admitted captures (the paced-refresh
    /// ceiling).
    pub min_interval: Duration,
    /// The most frames that may be in flight — encoded, not yet released — at
    /// once.
    pub max_in_flight: u32,
    /// The most retained encoded frame bytes across all in-flight frames at once.
    pub memory_budget: usize,
}

impl CaptureLimits {
    /// The protocol-documented defaults
    /// ([`limits::MIN_DESKTOP_CAPTURE_INTERVAL_MS`],
    /// [`limits::MAX_DESKTOP_IN_FLIGHT`],
    /// [`limits::MAX_DESKTOP_CAPTURE_MEMORY_BYTES`]).
    #[must_use]
    pub fn from_protocol() -> Self {
        Self {
            min_interval: Duration::from_millis(limits::MIN_DESKTOP_CAPTURE_INTERVAL_MS),
            max_in_flight: limits::MAX_DESKTOP_IN_FLIGHT,
            memory_budget: limits::MAX_DESKTOP_CAPTURE_MEMORY_BYTES,
        }
    }
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self::from_protocol()
    }
}

/// Which backpressure bound refused a capture (see [`Admission::Reject`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureReject {
    /// Too many frames are already in flight
    /// ([`CaptureLimits::max_in_flight`]).
    InFlight,
    /// Admitting the frame would exceed the memory budget
    /// ([`CaptureLimits::memory_budget`]).
    Memory,
}

/// The governor's decision for one capture request (REQ-N-006).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The capture may proceed now; the caller pairs it with
    /// [`CaptureGovernor::begin`].
    Admit,
    /// The capture is arriving faster than the refresh ceiling: wait this long,
    /// then proceed. Coalesces a burst down to the paced rate; the duration is
    /// never longer than [`CaptureLimits::min_interval`].
    Pace(Duration),
    /// The in-flight or memory budget is exhausted; the capture is refused
    /// (dropped) rather than queued without bound.
    Reject(CaptureReject),
}

/// The per-session desktop-capture backpressure governor (REQ-N-006,
/// ST-M005-001).
///
/// It holds the state the [`CaptureLimits`] are enforced against for one active
/// session — the last admitted capture instant (for pacing), the in-flight frame
/// count, and the retained frame bytes. It makes no OS calls and takes the
/// wall-clock instant as an argument, so every decision is deterministic and
/// unit-tested on every platform. The session runtime consults [`Self::admit`]
/// before each capture, calls [`Self::begin`] when it proceeds, [`Self::charge`]
/// with the encoded frame size, and [`Self::release`] once the frame has been
/// handed to the transport.
#[derive(Debug)]
pub struct CaptureGovernor {
    limits: CaptureLimits,
    last_admit: Option<Instant>,
    in_flight: u32,
    bytes_held: usize,
}

impl CaptureGovernor {
    /// Creates a governor enforcing `limits`, with no capture yet admitted.
    #[must_use]
    pub fn new(limits: CaptureLimits) -> Self {
        Self {
            limits,
            last_admit: None,
            in_flight: 0,
            bytes_held: 0,
        }
    }

    /// Decides whether a capture may proceed at `now`, without mutating state.
    ///
    /// A hard refusal wins over pacing: the in-flight and memory budgets are
    /// checked first ([`Admission::Reject`]), then the refresh interval
    /// ([`Admission::Pace`]), else [`Admission::Admit`].
    #[must_use]
    pub fn admit(&self, now: Instant) -> Admission {
        if self.in_flight >= self.limits.max_in_flight {
            return Admission::Reject(CaptureReject::InFlight);
        }
        if self.bytes_held >= self.limits.memory_budget {
            return Admission::Reject(CaptureReject::Memory);
        }
        if let Some(last) = self.last_admit {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < self.limits.min_interval {
                return Admission::Pace(self.limits.min_interval - elapsed);
            }
        }
        Admission::Admit
    }

    /// Records that an admitted capture has started at `now`: advances the pacing
    /// clock and increments the in-flight count. Call once per
    /// [`Admission::Admit`] (or once the paced wait has elapsed), before the
    /// capture runs, so pacing and the in-flight count reflect a capture
    /// regardless of whether it ends up carrying an image.
    pub fn begin(&mut self, now: Instant) {
        self.last_admit = Some(now);
        self.in_flight = self.in_flight.saturating_add(1);
    }

    /// Charges `bytes` of a just-encoded frame against the memory budget while it
    /// is held for transport (0 for a non-`Active` frame that carried no image).
    /// Paired with the same `bytes` later returned by [`Self::release`].
    pub fn charge(&mut self, bytes: usize) {
        self.bytes_held = self.bytes_held.saturating_add(bytes);
    }

    /// Records that an in-flight capture has been released to the transport:
    /// returns its `bytes` to the memory budget and decrements the in-flight
    /// count. Call once per [`Self::begin`], with the same `bytes` passed to
    /// [`Self::charge`] (or 0 if the capture failed before it was charged).
    pub fn release(&mut self, bytes: usize) {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.bytes_held = self.bytes_held.saturating_sub(bytes);
    }

    /// The bounds this governor enforces.
    #[must_use]
    pub fn limits(&self) -> CaptureLimits {
        self.limits
    }
}

/// The rate bounds a [`InputGovernor`] enforces on synthesized desktop input
/// (ST-M005-002, REQ-N-006/014).
#[derive(Debug, Clone, Copy)]
pub struct InputLimits {
    /// The sustained floor spacing between two admitted input actions.
    pub min_interval: Duration,
    /// The most actions that may fire back-to-back before pacing engages.
    pub burst: u32,
}

impl InputLimits {
    /// The protocol-documented defaults
    /// ([`limits::MIN_DESKTOP_INPUT_INTERVAL_MS`],
    /// [`limits::MAX_DESKTOP_INPUT_BURST`]).
    #[must_use]
    pub fn from_protocol() -> Self {
        Self {
            min_interval: Duration::from_millis(limits::MIN_DESKTOP_INPUT_INTERVAL_MS),
            burst: limits::MAX_DESKTOP_INPUT_BURST,
        }
    }
}

impl Default for InputLimits {
    fn default() -> Self {
        Self::from_protocol()
    }
}

/// The governor's decision for one input action (ST-M005-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAdmission {
    /// The action may fire now; the caller pairs it with
    /// [`InputGovernor::begin`].
    Admit,
    /// The action is arriving faster than the sustained ceiling once the burst
    /// allowance is spent: wait this long, then fire. Input is never dropped —
    /// a click delayed is still a click; only a flood is slowed. The duration is
    /// never longer than [`InputLimits::min_interval`].
    Pace(Duration),
}

/// The per-session desktop-input rate governor (REQ-N-006, ST-M005-002).
///
/// It paces synthesized pointer, scroll, key, type, and hotkey actions so a
/// runaway automation loop cannot drive `SendInput` on the customer's live
/// desktop without bound, while a normal operator sequence passes untouched. The
/// implementation is the Generic Cell Rate Algorithm (a leaky bucket expressed
/// as a single theoretical-arrival-time instant): exact, float-free, and — like
/// [`CaptureGovernor`] — free of OS calls, taking `now` as an argument so every
/// decision is deterministic and unit-tested on every platform. The runtime
/// consults [`Self::admit`] before each input action and calls [`Self::begin`]
/// when it fires (after any paced wait).
#[derive(Debug)]
pub struct InputGovernor {
    limits: InputLimits,
    /// The theoretical arrival time of the next conforming action, or `None`
    /// before the first action. Everything up to `tat - tolerance` may fire now.
    tat: Option<Instant>,
}

impl InputGovernor {
    /// Creates a governor enforcing `limits`, with the full burst allowance and
    /// no action yet admitted.
    #[must_use]
    pub fn new(limits: InputLimits) -> Self {
        Self { limits, tat: None }
    }

    /// The burst tolerance: how far ahead of the sustained schedule an action may
    /// arrive. `(burst - 1)` whole intervals, so `burst == 1` is pure
    /// min-interval pacing with no tolerance.
    fn tolerance(&self) -> Duration {
        self.limits
            .min_interval
            .saturating_mul(self.limits.burst.saturating_sub(1))
    }

    /// Decides whether an input action may fire at `now`, without mutating state.
    ///
    /// Admits immediately while the action is within the burst tolerance of the
    /// schedule; otherwise paces it to the sustained ceiling. Never rejects:
    /// input is lossless to defer.
    #[must_use]
    pub fn admit(&self, now: Instant) -> InputAdmission {
        match self.tat {
            None => InputAdmission::Admit,
            Some(tat) => {
                // `now` conforms when `now >= tat - tolerance`, i.e. the schedule
                // is no more than `tolerance` ahead of `now`. Compared as a
                // duration to avoid any `Instant - Duration` underflow.
                let ahead = tat.saturating_duration_since(now);
                let tolerance = self.tolerance();
                if ahead <= tolerance {
                    InputAdmission::Admit
                } else {
                    InputAdmission::Pace(ahead - tolerance)
                }
            }
        }
    }

    /// Records that an action fired at `now`: advances the theoretical arrival
    /// time by one interval from whichever is later, `now` or the current
    /// schedule. Call once per action, after any [`InputAdmission::Pace`] wait
    /// has elapsed.
    pub fn begin(&mut self, now: Instant) {
        let base = match self.tat {
            Some(tat) => tat.max(now),
            None => now,
        };
        self.tat = Some(base + self.limits.min_interval);
    }

    /// The bounds this governor enforces.
    #[must_use]
    pub fn limits(&self) -> InputLimits {
        self.limits
    }
}

/// The agent-side desktop capture-and-input backend.
///
/// Constructed once per active session by the runtime when the desktop
/// capability is in force. It holds no OS handles: each call opens and releases
/// its own transient device contexts and input synthesis, so the type is a
/// zero-sized, trivially `Send`/`Sync` marker that dispatches into the
/// platform backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Desktop {
    _private: (),
}

impl Desktop {
    /// Creates a desktop backend handle.
    #[must_use]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Whether a real capture-and-input backend is compiled in on this platform.
    ///
    /// The support agent consults this to decide whether to advertise
    /// `desktop.v1`: it advertises only where the backend exists, so a peer never
    /// sees the capability on a platform that would answer every desktop
    /// operation with [`ExecError::DesktopUnsupported`] (REQ-S-009, REQ-N-012).
    /// The Windows backend is present; every other platform has none.
    #[must_use]
    pub fn backend_available() -> bool {
        cfg!(windows)
    }

    /// Captures the current desktop frame for `request`.
    pub fn capture(&self, request: &CaptureRequest) -> Result<CapturedFrame, ExecError> {
        platform::capture(request)
    }

    /// Synthesizes a pointer action at a virtual-desktop coordinate.
    pub fn pointer(
        &self,
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<(), ExecError> {
        platform::pointer(action, button, x, y)
    }

    /// Synthesizes a scroll at a virtual-desktop coordinate.
    pub fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), ExecError> {
        platform::scroll(x, y, dx, dy)
    }

    /// Synthesizes a key action for one named virtual key.
    pub fn key(&self, action: KeyAction, key: &str) -> Result<(), ExecError> {
        platform::key(action, key)
    }

    /// Types a run of Unicode text as synthesized keystrokes.
    pub fn type_text(&self, text: &str) -> Result<(), ExecError> {
        platform::type_text(text)
    }

    /// Presses a chord of named virtual keys together (a hotkey).
    pub fn hotkey(&self, keys: &[String]) -> Result<(), ExecError> {
        platform::hotkey(keys)
    }

    /// Reads the current clipboard text (empty string when the clipboard holds
    /// no Unicode text).
    pub fn clipboard_get(&self) -> Result<String, ExecError> {
        platform::clipboard_get()
    }

    /// Replaces the clipboard text.
    pub fn clipboard_set(&self, text: &str) -> Result<(), ExecError> {
        platform::clipboard_set(text)
    }
}

// --- Cross-platform pure helpers (unit-tested on every platform) ---

/// Encodes a top-down RGBA8 buffer as a lossless PNG (ADR-0015). `rgba` must be
/// exactly `width * height * 4` bytes. The encoder is pure Rust, so the same
/// code path runs and is tested on every platform even though only the Windows
/// backend feeds it real pixels.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows backend calls it outside tests
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ExecError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4));
    if expected != Some(rgba.len()) {
        return Err(ExecError::DesktopBackend(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "rgba buffer size does not match the frame dimensions",
        )));
    }
    let mut buffer = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buffer, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|err| ExecError::DesktopBackend(png_io_error(&err)))?;
        writer
            .write_image_data(rgba)
            .map_err(|err| ExecError::DesktopBackend(png_io_error(&err)))?;
    }
    Ok(buffer)
}

/// Collapses a PNG encoding error into a content-free [`std::io::Error`]: the
/// message names the failure without echoing any pixel data (REQ-N-015).
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows backend calls it outside tests
fn png_io_error(err: &png::EncodingError) -> std::io::Error {
    std::io::Error::other(format!("png encode failed: {err}"))
}

/// Converts a top-down 32-bit BGRA (GDI's native `BI_RGB` order) buffer with a
/// possible row `stride` (bytes per row, `>= width * 4`) into a tightly packed
/// top-down RGBA8 buffer with an opaque alpha. GDI screen captures carry no
/// meaningful alpha, so it is forced to `0xFF` rather than trusted.
#[must_use]
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows backend calls it outside tests
pub fn bgra_to_rgba(bgra: &[u8], width: u32, height: u32, stride: usize) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = y * stride;
        let dst_row = y * w * 4;
        for x in 0..w {
            let s = row + x * 4;
            let d = dst_row + x * 4;
            if s + 3 < bgra.len() {
                out[d] = bgra[s + 2]; // R <- B-position source is BGRA: [B,G,R,A]
                out[d + 1] = bgra[s + 1]; // G
                out[d + 2] = bgra[s]; // B
                out[d + 3] = 0xFF; // opaque
            } else {
                out[d + 3] = 0xFF;
            }
        }
    }
    out
}

/// Maps a virtual-desktop pixel coordinate to the `0..=65535` normalized
/// absolute range `SendInput` uses with `MOUSEEVENTF_ABSOLUTE |
/// MOUSEEVENTF_VIRTUALDESK`. `origin` is the virtual screen's left/top edge
/// (`SM_XVIRTUALSCREEN`/`SM_YVIRTUALSCREEN`, which may be negative) and `span`
/// its width/height (`SM_CXVIRTUALSCREEN`/`SM_CYVIRTUALSCREEN`). The result is
/// clamped into range so an out-of-bounds coordinate lands on the nearest edge
/// rather than wrapping.
#[must_use]
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows backend calls it outside tests
pub fn normalize_virtual(coord: i32, origin: i32, span: i32) -> i32 {
    if span <= 1 {
        return 0;
    }
    let offset = i64::from(coord) - i64::from(origin);
    let scaled = (offset * 65535) / i64::from(span - 1);
    scaled.clamp(0, 65535) as i32
}

/// Maps a key name to a Windows virtual-key code, or `None` when the name is not
/// a recognized key. The names are stable, case-insensitive tokens the console
/// and AI use (`"enter"`, `"tab"`, `"f5"`, `"left"`, `"ctrl"`, `"a"`, `"1"`),
/// decoupling the wire vocabulary from the OS constants. Single printable ASCII
/// letters and digits map to their `VK_*` codes directly.
#[must_use]
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows backend calls it outside tests
pub fn key_name_to_vk(name: &str) -> Option<u16> {
    let key = name.trim().to_ascii_lowercase();
    // Single ASCII letter or digit: VK codes equal the uppercase ASCII byte.
    if key.len() == 1 {
        let c = key.as_bytes()[0];
        if c.is_ascii_lowercase() {
            return Some(u16::from(c.to_ascii_uppercase()));
        }
        if c.is_ascii_digit() {
            return Some(u16::from(c));
        }
    }
    let vk = match key.as_str() {
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "space" | "spacebar" => 0x20,
        "backspace" | "back" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" | "prior" => 0x21,
        "pagedown" | "pgdn" | "next" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "ctrl" | "control" => 0x11,
        "alt" | "menu" => 0x12,
        "shift" => 0x10,
        "win" | "meta" | "super" | "lwin" => 0x5B,
        "rwin" => 0x5C,
        "apps" | "menu_key" => 0x5D,
        "capslock" | "caps" => 0x14,
        "printscreen" | "prtsc" | "snapshot" => 0x2C,
        "scrolllock" => 0x91,
        "pause" | "break" => 0x13,
        "numlock" => 0x90,
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f7" => 0x76,
        "f8" => 0x77,
        "f9" => 0x78,
        "f10" => 0x79,
        "f11" => 0x7A,
        "f12" => 0x7B,
        "minus" | "-" => 0xBD,
        "equals" | "=" => 0xBB,
        "plus" | "+" => 0xBB,
        "comma" | "," => 0xBC,
        "period" | "." => 0xBE,
        "slash" | "/" => 0xBF,
        "backslash" | "\\" => 0xDC,
        "semicolon" | ";" => 0xBA,
        "quote" | "'" => 0xDE,
        "backtick" | "grave" | "`" => 0xC0,
        "lbracket" | "[" => 0xDB,
        "rbracket" | "]" => 0xDD,
        _ => return None,
    };
    Some(vk)
}

#[cfg(not(windows))]
mod platform {
    //! Off-Windows stub: no capture-and-input backend exists, so every call
    //! fails closed. A production agent on these platforms does not advertise
    //! `desktop.v1` (see [`super::Desktop::backend_available`]), so this is
    //! reached only by the session end-to-end suite forcing the capability on to
    //! exercise the honest unsupported path.

    use super::{CaptureRequest, CapturedFrame};
    use crate::error::ExecError;
    use cretlink_protocol::{KeyAction, PointerAction, PointerButton};

    pub(super) fn capture(_request: &CaptureRequest) -> Result<CapturedFrame, ExecError> {
        Err(ExecError::DesktopUnsupported)
    }
    pub(super) fn pointer(
        _action: PointerAction,
        _button: PointerButton,
        _x: i32,
        _y: i32,
    ) -> Result<(), ExecError> {
        Err(ExecError::DesktopUnsupported)
    }
    pub(super) fn scroll(_x: i32, _y: i32, _dx: i32, _dy: i32) -> Result<(), ExecError> {
        Err(ExecError::DesktopUnsupported)
    }
    pub(super) fn key(_action: KeyAction, _key: &str) -> Result<(), ExecError> {
        Err(ExecError::DesktopUnsupported)
    }
    pub(super) fn type_text(_text: &str) -> Result<(), ExecError> {
        Err(ExecError::DesktopUnsupported)
    }
    pub(super) fn hotkey(_keys: &[String]) -> Result<(), ExecError> {
        Err(ExecError::DesktopUnsupported)
    }
    pub(super) fn clipboard_get() -> Result<String, ExecError> {
        Err(ExecError::DesktopUnsupported)
    }
    pub(super) fn clipboard_set(_text: &str) -> Result<(), ExecError> {
        Err(ExecError::DesktopUnsupported)
    }
}

#[cfg(windows)]
mod platform;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trips_a_small_frame() {
        // A 2x2 RGBA frame encodes and decodes back to the same pixels.
        let rgba = vec![
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
            255, 255, 0, 255, // yellow
        ];
        let png = encode_png(&rgba, 2, 2).expect("encode");
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().expect("read info");
        let mut out = vec![0u8; reader.output_buffer_size().expect("buffer size")];
        let info = reader.next_frame(&mut out).expect("frame");
        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);
        assert_eq!(&out[..rgba.len()], &rgba[..]);
    }

    #[test]
    fn png_rejects_a_mismatched_buffer() {
        let err = encode_png(&[0, 0, 0, 255], 2, 2).expect_err("size mismatch");
        assert!(matches!(err, ExecError::DesktopBackend(_)));
    }

    #[test]
    fn bgra_to_rgba_swaps_channels_and_forces_opaque() {
        // One BGRA pixel [B=10, G=20, R=30, A=0] becomes RGBA [30, 20, 10, 255].
        let bgra = vec![10, 20, 30, 0];
        let rgba = bgra_to_rgba(&bgra, 1, 1, 4);
        assert_eq!(rgba, vec![30, 20, 10, 255]);
    }

    #[test]
    fn bgra_to_rgba_honours_row_stride() {
        // 1x2 image, stride 8 (4 bytes of padding after each 1-pixel row).
        let bgra = vec![
            1, 2, 3, 0, 99, 99, 99, 99, // row 0 pixel + padding
            4, 5, 6, 0, 99, 99, 99, 99, // row 1 pixel + padding
        ];
        let rgba = bgra_to_rgba(&bgra, 1, 2, 8);
        assert_eq!(rgba, vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn normalize_virtual_maps_edges_and_clamps() {
        // Origin 0, span 1920: left edge -> 0, right edge -> 65535.
        assert_eq!(normalize_virtual(0, 0, 1920), 0);
        assert_eq!(normalize_virtual(1919, 0, 1920), 65535);
        // Negative origin (a monitor left of primary).
        assert_eq!(normalize_virtual(-1920, -1920, 3840), 0);
        // Out-of-range clamps to the nearest edge, never wraps.
        assert_eq!(normalize_virtual(-100, 0, 1920), 0);
        assert_eq!(normalize_virtual(9999, 0, 1920), 65535);
        // Degenerate span never divides by zero.
        assert_eq!(normalize_virtual(5, 0, 1), 0);
    }

    #[test]
    fn key_names_map_to_virtual_keys() {
        assert_eq!(key_name_to_vk("enter"), Some(0x0D));
        assert_eq!(key_name_to_vk("ENTER"), Some(0x0D));
        assert_eq!(key_name_to_vk("f5"), Some(0x74));
        assert_eq!(key_name_to_vk("a"), Some(0x41));
        assert_eq!(key_name_to_vk("Z"), Some(0x5A));
        assert_eq!(key_name_to_vk("7"), Some(0x37));
        assert_eq!(key_name_to_vk("ctrl"), Some(0x11));
        assert_eq!(key_name_to_vk("left"), Some(0x25));
        assert_eq!(key_name_to_vk(" tab "), Some(0x09));
        assert_eq!(key_name_to_vk("not-a-key"), None);
        assert_eq!(key_name_to_vk(""), None);
    }

    // --- Capture backpressure governor (REQ-N-006, ST-M005-001) ---
    //
    // The governor takes `now` as an argument and makes no OS calls, so every
    // decision below is deterministic on every platform in the matrix.

    /// A governor with generous room but a coarse interval, so pacing is the
    /// only bound in play unless a test drives the others.
    fn test_governor() -> CaptureGovernor {
        CaptureGovernor::new(CaptureLimits {
            min_interval: Duration::from_millis(50),
            max_in_flight: 3,
            memory_budget: 8 * 1024,
        })
    }

    #[test]
    fn from_protocol_mirrors_the_documented_limits() {
        let limits = CaptureLimits::from_protocol();
        assert_eq!(
            limits.min_interval,
            Duration::from_millis(limits::MIN_DESKTOP_CAPTURE_INTERVAL_MS)
        );
        assert_eq!(limits.max_in_flight, limits::MAX_DESKTOP_IN_FLIGHT);
        assert_eq!(
            limits.memory_budget,
            limits::MAX_DESKTOP_CAPTURE_MEMORY_BYTES
        );
        // `Default` and `from_protocol` agree.
        assert_eq!(CaptureLimits::default().min_interval, limits.min_interval);
    }

    #[test]
    fn first_capture_is_admitted_immediately() {
        let gov = test_governor();
        let now = Instant::now();
        assert_eq!(gov.admit(now), Admission::Admit);
    }

    #[test]
    fn a_burst_within_the_interval_is_paced_not_dropped() {
        let mut gov = test_governor();
        let start = Instant::now();
        assert_eq!(gov.admit(start), Admission::Admit);
        gov.begin(start);
        gov.charge(1024);
        gov.release(1024);
        // 10ms later, 40ms of the 50ms interval remain: pace, do not reject.
        let soon = start + Duration::from_millis(10);
        assert_eq!(gov.admit(soon), Admission::Pace(Duration::from_millis(40)));
    }

    #[test]
    fn a_capture_after_the_interval_is_admitted_again() {
        let mut gov = test_governor();
        let start = Instant::now();
        gov.begin(start);
        gov.release(0);
        // Exactly one interval later the pacing clock has drained.
        let later = start + Duration::from_millis(50);
        assert_eq!(gov.admit(later), Admission::Admit);
    }

    #[test]
    fn in_flight_budget_rejects_before_pacing() {
        let mut gov = test_governor();
        let now = Instant::now();
        // Fill the in-flight window without releasing.
        for _ in 0..3 {
            gov.begin(now);
        }
        assert_eq!(gov.admit(now), Admission::Reject(CaptureReject::InFlight));
        // Releasing one frame reopens admission (pacing aside).
        gov.release(0);
        assert_ne!(gov.admit(now), Admission::Reject(CaptureReject::InFlight));
    }

    #[test]
    fn memory_budget_rejects_when_held_bytes_reach_it() {
        let mut gov = test_governor();
        let now = Instant::now();
        gov.begin(now);
        gov.charge(8 * 1024); // exactly the budget is already too much to add to
        assert_eq!(gov.admit(now), Admission::Reject(CaptureReject::Memory));
        // Releasing the held bytes clears the memory refusal.
        gov.release(8 * 1024);
        assert_ne!(gov.admit(now), Admission::Reject(CaptureReject::Memory));
    }

    #[test]
    fn release_and_charge_saturate_rather_than_wrap() {
        let mut gov = test_governor();
        // Releasing with nothing in flight must not underflow to u32::MAX / a
        // huge byte count that would then wedge admission forever.
        gov.release(4096);
        let now = Instant::now();
        assert_eq!(gov.admit(now), Admission::Admit);
    }

    // --- Input rate governor (ST-M005-002) ---
    //
    // Like the capture governor, `InputGovernor` takes `now` as an argument and
    // makes no OS calls, so every decision below is deterministic on every
    // platform. A coarse interval and a small burst keep the arithmetic legible.

    /// A governor with a 10ms floor and a burst of 4 (30ms tolerance).
    fn test_input_governor() -> InputGovernor {
        InputGovernor::new(InputLimits {
            min_interval: Duration::from_millis(10),
            burst: 4,
        })
    }

    #[test]
    fn input_from_protocol_mirrors_the_documented_limits() {
        let limits = InputLimits::from_protocol();
        assert_eq!(
            limits.min_interval,
            Duration::from_millis(limits::MIN_DESKTOP_INPUT_INTERVAL_MS)
        );
        assert_eq!(limits.burst, limits::MAX_DESKTOP_INPUT_BURST);
        assert_eq!(InputLimits::default().burst, limits.burst);
    }

    #[test]
    fn first_input_action_is_admitted_immediately() {
        let gov = test_input_governor();
        assert_eq!(gov.admit(Instant::now()), InputAdmission::Admit);
    }

    #[test]
    fn a_burst_up_to_the_tolerance_fires_without_delay() {
        let mut gov = test_input_governor();
        let now = Instant::now();
        // Four back-to-back actions at the same instant are all admitted (the
        // burst allowance), each advancing the schedule by one interval.
        for _ in 0..4 {
            assert_eq!(gov.admit(now), InputAdmission::Admit);
            gov.begin(now);
        }
        // The fifth at the same instant is one whole interval past tolerance.
        assert_eq!(
            gov.admit(now),
            InputAdmission::Pace(Duration::from_millis(10))
        );
    }

    #[test]
    fn a_flood_is_paced_never_dropped() {
        let mut gov = test_input_governor();
        let now = Instant::now();
        // Spend the burst, then confirm every subsequent same-instant action is
        // paced (a positive wait) rather than refused — input is lossless.
        for _ in 0..4 {
            gov.begin(now);
        }
        match gov.admit(now) {
            InputAdmission::Pace(wait) => {
                assert!(wait > Duration::ZERO && wait <= Duration::from_millis(10));
            }
            InputAdmission::Admit => panic!("a flood past the burst must be paced"),
        }
    }

    #[test]
    fn pacing_drains_at_one_action_per_interval() {
        let mut gov = test_input_governor();
        let start = Instant::now();
        for _ in 0..4 {
            gov.begin(start);
        }
        // One interval after the burst, exactly one more action conforms.
        let later = start + Duration::from_millis(10);
        assert_eq!(gov.admit(later), InputAdmission::Admit);
    }

    #[test]
    fn burst_of_one_is_pure_min_interval_pacing() {
        let mut gov = InputGovernor::new(InputLimits {
            min_interval: Duration::from_millis(10),
            burst: 1,
        });
        let start = Instant::now();
        assert_eq!(gov.admit(start), InputAdmission::Admit);
        gov.begin(start);
        // With no tolerance, the very next same-instant action waits a full
        // interval.
        assert_eq!(
            gov.admit(start),
            InputAdmission::Pace(Duration::from_millis(10))
        );
    }

    #[test]
    fn a_long_idle_resets_the_schedule_without_over_crediting() {
        let mut gov = test_input_governor();
        let start = Instant::now();
        for _ in 0..4 {
            gov.begin(start);
        }
        // Long after the burst the schedule has fully drained: the next action
        // admits, and the burst allowance is *not* re-credited beyond one — a
        // second same-instant action is immediately admitted only up to the
        // burst again, proving `begin` clamps the schedule to `now`.
        let much_later = start + Duration::from_secs(1);
        assert_eq!(gov.admit(much_later), InputAdmission::Admit);
        gov.begin(much_later);
        // tat is now much_later + 10ms; a fresh burst is available from here.
        for _ in 0..3 {
            assert_eq!(gov.admit(much_later), InputAdmission::Admit);
            gov.begin(much_later);
        }
        assert_eq!(
            gov.admit(much_later),
            InputAdmission::Pace(Duration::from_millis(10))
        );
    }

    // --- Live Windows backend smoke tests (ST-M005-001) ---
    //
    // These touch the real desktop, so they are `#[ignore]`d: the maintainer
    // runs them explicitly on attended hardware
    // (`cargo test -p cretlink-exec -- --ignored live_windows`). They never
    // synthesize pointer or key input — only read-only capture and a clipboard
    // round-trip that restores what it found — so they cannot disturb the
    // session. They prove the backend answers honestly, not that pixels look a
    // certain way.

    #[cfg(windows)]
    #[test]
    #[ignore = "reads the live Windows desktop; run explicitly on attended hardware"]
    fn live_windows_backend_is_available() {
        assert!(
            Desktop::backend_available(),
            "the Windows build must report a real capture-and-input backend"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "reads the live Windows desktop; run explicitly on attended hardware"]
    fn live_windows_capture_is_honest() {
        use cretlink_protocol::{DesktopState, ImageFormat};

        let desktop = Desktop::new();
        let frame = desktop
            .capture(&CaptureRequest {
                display: 0,
                format: ImageFormat::Png,
                max_width: None,
                max_height: None,
                quality: None,
            })
            .expect("the backend must answer capture");

        if frame.meta.state == DesktopState::Active {
            let image = frame.image.expect("an Active desktop carries pixels");
            assert_eq!(
                &image[..8],
                &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'],
                "an Active frame is a real PNG"
            );
            assert!(frame.meta.physical_width > 0 && frame.meta.physical_height > 0);
        } else {
            assert!(
                frame.image.is_none(),
                "a non-Active desktop never carries a fabricated frame, state was {:?}",
                frame.meta.state
            );
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "mutates the live Windows clipboard (and restores it); run on attended hardware"]
    fn live_windows_clipboard_roundtrips() {
        let desktop = Desktop::new();
        // Preserve whatever the user had so the test leaves no trace.
        let original = desktop.clipboard_get().expect("read clipboard");

        let sentinel = "CretLink desktop.v1 clipboard round-trip ✓";
        desktop.clipboard_set(sentinel).expect("write clipboard");
        let read_back = desktop.clipboard_get().expect("read clipboard back");
        assert_eq!(read_back, sentinel, "clipboard must round-trip Unicode");

        // Restore the user's original clipboard content.
        desktop.clipboard_set(&original).expect("restore clipboard");
    }
}
