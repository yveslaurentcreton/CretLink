//! Live system inspection for the agent's `SystemInfo` answers (ST-M003-003,
//! REQ-F-016).
//!
//! Three facts are read from the real machine at call time — the Windows
//! version and build, the **native** machine architecture, and a best-effort
//! pending-reboot signal — through the safe API of the `winreg` crate, so this
//! module needs no `unsafe` (the workspace denies it; only a few reviewed FFI
//! modules carry an exception). Every query degrades honestly: a failed read
//! yields an explicit fallback, never an invented value (REQ-N-008). On
//! non-Windows hosts (the macOS and Linux CI matrix) the module returns
//! deterministic host-tag values, mirroring how [`crate::privilege`] degrades.
//!
//! # Sources
//!
//! - Version: `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` —
//!   `ProductName`, `DisplayVersion` (falling back to the older `ReleaseId`),
//!   `CurrentBuildNumber`, and `UBR`, composed into for example
//!   `"Windows 11 Pro 24H2 (build 26100.1234)"`.
//! - Architecture: `HKLM\SYSTEM\CurrentControlSet\Control\Session
//!   Manager\Environment` → `PROCESSOR_ARCHITECTURE`. This is the machine's
//!   native architecture (`HKLM\SYSTEM` is never WOW64-redirected and the
//!   value is not the per-process environment), so it stays truthful even
//!   under emulation, unlike the compile-time constant.
//! - Reboot pending: an OR over the three standard signals (Component Based
//!   Servicing `RebootPending`, Windows Update `RebootRequired`, and a
//!   non-empty `PendingFileRenameOperations`). `false` means "no signal
//!   observed" — the probe is documented best-effort, never a guarantee.

/// The composed operating-system version string, read live from the registry
/// on Windows. Off Windows this is the deterministic host OS tag (the agent
/// is a Windows product; CI merely builds everywhere).
#[must_use]
pub fn os_version() -> String {
    #[cfg(windows)]
    {
        windows_registry::os_version()
    }
    #[cfg(not(windows))]
    {
        std::env::consts::OS.to_string()
    }
}

/// The native machine architecture (`x86_64`, `aarch64`, ...), detected at
/// runtime on Windows. Off Windows this is the deterministic compile-time
/// architecture of the host.
#[must_use]
pub fn machine_arch() -> String {
    #[cfg(windows)]
    {
        windows_registry::machine_arch()
    }
    #[cfg(not(windows))]
    {
        std::env::consts::ARCH.to_string()
    }
}

/// Whether the machine reports a pending reboot, probed live (never cached).
/// Best-effort: `true` when any standard signal is present, `false` when no
/// signal was observed — including when a probe itself failed (REQ-N-008: an
/// absent signal is reported as such, not dressed up either way). Off Windows
/// this is deterministically `false`.
#[must_use]
pub fn reboot_pending() -> bool {
    #[cfg(windows)]
    {
        windows_registry::reboot_pending()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Composes the version string from the registry facts, tolerating any subset
/// being unavailable. Pure so it is unit-tested on every CI platform.
///
/// The registry famously keeps `ProductName` at "Windows 10 ..." on Windows
/// 11, so a product name is corrected from the build number (Windows 11 starts
/// at build 22000) — reporting what the machine actually runs (REQ-N-008).
// Only the Windows reader calls this outside of tests; the cfg keeps the
// non-Windows lib build free of dead code while the tests still run everywhere.
#[cfg(any(windows, test))]
fn compose_os_version(
    product: Option<String>,
    display: Option<String>,
    build: Option<String>,
    ubr: Option<u32>,
) -> String {
    let mut name = product.unwrap_or_else(|| "Windows".to_string());
    if let Some(build) = &build
        && let Ok(number) = build.parse::<u64>()
        && number >= 22_000
        && let Some(rest) = name.strip_prefix("Windows 10")
    {
        name = format!("Windows 11{rest}");
    }
    let mut out = name;
    if let Some(display) = display {
        out.push(' ');
        out.push_str(&display);
    }
    match (build, ubr) {
        (Some(build), Some(ubr)) => {
            out.push_str(&format!(" (build {build}.{ubr})"));
        }
        (Some(build), None) => {
            out.push_str(&format!(" (build {build})"));
        }
        // No build number could be read: say so rather than fake one.
        (None, _) => out.push_str(" (build unavailable)"),
    }
    out
}

/// Maps a Windows `PROCESSOR_ARCHITECTURE` token onto the Rust-style
/// architecture vocabulary the wire identity uses. Unknown tokens pass
/// through lowercased rather than being guessed at. Pure and unit-tested on
/// every CI platform.
#[cfg(any(windows, test))]
fn normalize_arch(raw: &str) -> String {
    match raw.to_ascii_uppercase().as_str() {
        "AMD64" => "x86_64".to_string(),
        "ARM64" => "aarch64".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

#[cfg(windows)]
mod windows_registry {
    //! The live registry reads. Zero `unsafe`: `winreg` wraps the Win32
    //! registry API in safe Rust.

    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    use super::{compose_os_version, normalize_arch};

    /// The version facts live here.
    const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    /// The machine-wide environment holds the native architecture.
    const SESSION_ENVIRONMENT: &str =
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
    /// Present when Component Based Servicing wants a reboot.
    const CBS_REBOOT_PENDING: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending";
    /// Present when Windows Update wants a reboot.
    const WU_REBOOT_REQUIRED: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired";
    /// Holds `PendingFileRenameOperations` when file renames await a reboot.
    const SESSION_MANAGER: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager";

    pub(super) fn os_version() -> String {
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let Ok(key) = hklm.open_subkey(CURRENT_VERSION) else {
            // The key itself was unreadable: still never the bare "Windows".
            return compose_os_version(None, None, None, None);
        };
        let product: Option<String> = key.get_value("ProductName").ok();
        // `DisplayVersion` (for example "24H2") arrived with Windows 10 20H2;
        // older builds carry `ReleaseId` (for example "1909") instead.
        let display: Option<String> = key
            .get_value("DisplayVersion")
            .ok()
            .or_else(|| key.get_value("ReleaseId").ok());
        let build: Option<String> = key.get_value("CurrentBuildNumber").ok();
        let ubr: Option<u32> = key.get_value("UBR").ok();
        compose_os_version(product, display, build, ubr)
    }

    pub(super) fn machine_arch() -> String {
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        hklm.open_subkey(SESSION_ENVIRONMENT)
            .and_then(|key| key.get_value::<String, _>("PROCESSOR_ARCHITECTURE"))
            .map(|raw| normalize_arch(&raw))
            // The registry read failed: the compile-time architecture is the
            // honest remaining knowledge (ADR-0002 ships x64-only anyway).
            .unwrap_or_else(|_| std::env::consts::ARCH.to_string())
    }

    pub(super) fn reboot_pending() -> bool {
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        // A best-effort OR: each probe that fails simply contributes "no
        // signal". Short-circuits on the first positive signal.
        hklm.open_subkey(CBS_REBOOT_PENDING).is_ok()
            || hklm.open_subkey(WU_REBOOT_REQUIRED).is_ok()
            || pending_file_renames(&hklm)
    }

    /// Whether `PendingFileRenameOperations` exists and holds actual entries
    /// (the REG_MULTI_SZ can linger empty; all-NUL data is not a signal).
    fn pending_file_renames(hklm: &RegKey) -> bool {
        hklm.open_subkey(SESSION_MANAGER)
            .and_then(|key| key.get_raw_value("PendingFileRenameOperations"))
            .map(|value| value.bytes.iter().any(|byte| *byte != 0))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_reports_full_version_with_build_and_ubr() {
        let version = compose_os_version(
            Some("Windows 11 Pro".to_string()),
            Some("24H2".to_string()),
            Some("26100".to_string()),
            Some(1234),
        );
        assert_eq!(version, "Windows 11 Pro 24H2 (build 26100.1234)");
    }

    #[test]
    fn compose_corrects_the_windows_10_product_name_on_11_builds() {
        // The registry keeps "Windows 10 ..." on Windows 11; builds >= 22000
        // are Windows 11 and must be reported as such.
        let version = compose_os_version(
            Some("Windows 10 Pro".to_string()),
            Some("24H2".to_string()),
            Some("26100".to_string()),
            Some(1),
        );
        assert_eq!(version, "Windows 11 Pro 24H2 (build 26100.1)");

        // A genuine Windows 10 build keeps its name.
        let version = compose_os_version(
            Some("Windows 10 Pro".to_string()),
            Some("22H2".to_string()),
            Some("19045".to_string()),
            Some(5011),
        );
        assert_eq!(version, "Windows 10 Pro 22H2 (build 19045.5011)");
    }

    #[test]
    fn compose_degrades_honestly_without_optional_facts() {
        assert_eq!(
            compose_os_version(None, None, Some("26100".to_string()), None),
            "Windows (build 26100)"
        );
        // Nothing readable at all: still never the bare "Windows" literal.
        assert_eq!(
            compose_os_version(None, None, None, None),
            "Windows (build unavailable)"
        );
    }

    #[test]
    fn arch_tokens_map_to_the_wire_vocabulary() {
        assert_eq!(normalize_arch("AMD64"), "x86_64");
        assert_eq!(normalize_arch("amd64"), "x86_64");
        assert_eq!(normalize_arch("ARM64"), "aarch64");
        assert_eq!(normalize_arch("x86"), "x86");
        assert_eq!(normalize_arch("IA64"), "ia64");
    }

    // --- Windows-only live probes (ST-M003-003 verification) ---

    /// The live version carries a real build number and is never the bare
    /// literal `Windows`.
    #[cfg(windows)]
    #[test]
    fn live_os_version_contains_a_build_number() {
        let version = os_version();
        assert_ne!(version, "Windows");
        assert!(
            version.contains("build "),
            "expected a build number in {version:?}"
        );
        assert!(
            version.chars().any(|c| c.is_ascii_digit()),
            "expected digits in {version:?}"
        );
    }

    /// The runtime-detected machine architecture is one the agent supports
    /// reporting (ADR-0002 ships x64-only; ARM64 machines still report
    /// truthfully).
    #[cfg(windows)]
    #[test]
    fn live_machine_arch_is_a_known_token() {
        let arch = machine_arch();
        assert!(
            arch == "x86_64" || arch == "aarch64",
            "unexpected machine architecture {arch:?}"
        );
    }

    /// The reboot probe answers without panicking; either value is legal.
    #[cfg(windows)]
    #[test]
    fn live_reboot_probe_answers_without_panic() {
        let pending = reboot_pending();
        // Exercise both the value and a second, independent probe run (the
        // probe is stateless and live).
        assert_eq!(pending, reboot_pending());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_values_are_deterministic_host_tags() {
        assert_eq!(os_version(), std::env::consts::OS);
        assert_eq!(machine_arch(), std::env::consts::ARCH);
        assert!(!reboot_pending());
    }
}
