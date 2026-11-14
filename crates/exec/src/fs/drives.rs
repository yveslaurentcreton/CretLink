//! The drives of the target (ST-M010-001).
//!
//! [`list`] answers [`cretlink_protocol::Operation::FsDrives`]: every drive
//! with its root folder, its kind, and — where the system can say — its label
//! and its total and free space. It reads those facts and nothing else.
//!
//! # Honest gaps (REQ-N-008)
//!
//! A label or a size the system cannot give is **absent**, never zero and
//! never guessed: a card reader without a card has a kind and nothing more.
//! A drive on the network is listed with its kind only, on purpose: asking a
//! file server for its label and space takes as long as the network takes, and
//! this operation runs while the agent serves nothing else.
//!
//! # Platforms
//!
//! On Windows the drives are the drive letters in use. Elsewhere — a
//! development agent or the simulated target, never a customer computer —
//! there are no drive letters, and the one entry is the root file system,
//! `/`. That root is not a path the wire accepts for another file operation
//! (the wire accepts drive-qualified paths only); it is reported as the
//! truthful answer of such a host.

use cretlink_protocol::{DriveEntry, limits};

use super::one_line;

/// Lists the drives of this computer, every entry within the wire bounds.
///
/// Blocking: it asks the operating system about each drive, so the caller
/// runs it off the async runtime.
pub(super) fn list() -> Vec<DriveEntry> {
    sys::list()
        .into_iter()
        .filter_map(fit)
        .take(limits::MAX_DRIVES)
        .collect()
}

/// Brings one drive within the wire bounds, so that the answer can always be
/// sent: a label is cut to its bound and kept on one line, and a size outside
/// the range the wire carries, or a free space larger than the drive, is
/// reported as not known. A drive whose root is not a usable text is left out.
fn fit(mut drive: DriveEntry) -> Option<DriveEntry> {
    if drive.root.is_empty()
        || drive.root.len() > limits::MAX_DRIVE_ROOT_BYTES
        || one_line(&drive.root, limits::MAX_DRIVE_ROOT_BYTES) != drive.root
    {
        return None;
    }
    drive.label = drive
        .label
        .map(|label| one_line(&label, limits::MAX_DRIVE_LABEL_BYTES))
        .filter(|label| !label.is_empty());
    drive.total_bytes = drive
        .total_bytes
        .filter(|total| *total <= limits::MAX_FILE_OFFSET);
    let most_free = drive.total_bytes.unwrap_or(limits::MAX_FILE_OFFSET);
    drive.free_bytes = drive.free_bytes.filter(|free| *free <= most_free);
    Some(drive)
}

#[cfg(windows)]
mod sys {
    #![allow(
        unsafe_code,
        reason = "Localized Win32 volume queries for the drive list \
                  (ST-M010-001); ADR-0001 and ADR-0006 permit reviewed, \
                  localized unsafe in one platform adapter."
    )]

    use cretlink_protocol::{DriveEntry, DriveKind};
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows::Win32::System::Diagnostics::Debug::{
        GetThreadErrorMode, SEM_FAILCRITICALERRORS, SetThreadErrorMode, THREAD_ERROR_MODE,
    };
    use windows::core::PCWSTR;

    // What `GetDriveTypeW` answers (winbase.h).
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    /// The longest volume label `GetVolumeInformationW` writes, with its
    /// terminator (`MAX_PATH + 1`).
    const LABEL_UNITS: usize = 261;

    /// The drive letters in use, each with what the system says about it.
    pub(super) fn list() -> Vec<DriveEntry> {
        // A drive without a medium must answer "not ready", not open a system
        // dialog on the customer's screen that waits for a click.
        let _quiet = QuietErrors::enter();
        // SAFETY: the call takes no argument and returns a bit mask of the
        // drive letters in use.
        let letters = unsafe { GetLogicalDrives() };
        let mut drives = Vec::new();
        for index in 0..26u8 {
            if letters & (1u32 << index) == 0 {
                continue;
            }
            let root = format!("{}:\\", char::from(b'A' + index));
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `wide` is a NUL-terminated UTF-16 string that lives
            // through the call.
            let kind = match unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) } {
                // The letter is in the mask but nothing is mounted there.
                DRIVE_NO_ROOT_DIR => continue,
                DRIVE_REMOVABLE => DriveKind::Removable,
                DRIVE_FIXED => DriveKind::Fixed,
                DRIVE_REMOTE => DriveKind::Network,
                DRIVE_CDROM => DriveKind::Optical,
                DRIVE_RAMDISK => DriveKind::RamDisk,
                _ => DriveKind::Unknown,
            };
            // A file server is not asked: see the module documentation.
            let (label, (total_bytes, free_bytes)) = if kind == DriveKind::Network {
                (None, (None, None))
            } else {
                (label(&wide), space(&wide))
            };
            drives.push(DriveEntry {
                root,
                kind,
                label,
                total_bytes,
                free_bytes,
            });
        }
        drives
    }

    /// The volume label of the drive at `root`, when it has one that can be
    /// read. `root` is NUL-terminated.
    fn label(root: &[u16]) -> Option<String> {
        let mut name = [0u16; LABEL_UNITS];
        // SAFETY: `root` is NUL-terminated and lives through the call; `name`
        // is a writable buffer whose length the wrapper passes with it; no
        // other output is asked for.
        unsafe {
            GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                Some(&mut name),
                None,
                None,
                None,
                None,
            )
        }
        .ok()?;
        let len = name
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(name.len());
        let label = String::from_utf16_lossy(&name[..len]);
        (!label.is_empty()).then_some(label)
    }

    /// The size of the drive at `root` and the bytes free for this account,
    /// when the drive can say. `root` is NUL-terminated.
    fn space(root: &[u16]) -> (Option<u64>, Option<u64>) {
        let (mut free, mut total) = (0u64, 0u64);
        // SAFETY: `root` is NUL-terminated and lives through the call; the two
        // out pointers are valid for the call, and the third is not asked for.
        let asked = unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(root.as_ptr()),
                Some(&raw mut free),
                Some(&raw mut total),
                None,
            )
        };
        match asked {
            Ok(()) => (Some(total), Some(free)),
            Err(_) => (None, None),
        }
    }

    /// Keeps the system's critical-error dialog ("there is no disk in the
    /// drive") from this thread while the drives are asked, and puts the
    /// thread's error mode back afterwards. The mode is the thread's own, so
    /// nothing else in the process is affected.
    struct QuietErrors {
        previous: Option<THREAD_ERROR_MODE>,
    }

    impl QuietErrors {
        fn enter() -> Self {
            // SAFETY: the call takes no argument and reads this thread's mode.
            let previous = THREAD_ERROR_MODE(unsafe { GetThreadErrorMode() });
            let quiet = THREAD_ERROR_MODE(previous.0 | SEM_FAILCRITICALERRORS.0);
            // SAFETY: the mode is a plain value and no previous mode is asked
            // for; the call changes this thread's mode only.
            let set = unsafe { SetThreadErrorMode(quiet, None) };
            Self {
                previous: set.is_ok().then_some(previous),
            }
        }
    }

    impl Drop for QuietErrors {
        fn drop(&mut self) {
            if let Some(previous) = self.previous {
                // SAFETY: as in `enter`; this restores the mode it replaced.
                let _ = unsafe { SetThreadErrorMode(previous, None) };
            }
        }
    }
}

#[cfg(unix)]
mod sys {
    use cretlink_protocol::{DriveEntry, DriveKind};

    /// The one "drive" of a host without drive letters: its root file system.
    pub(super) fn list() -> Vec<DriveEntry> {
        let (total_bytes, free_bytes) = match nix::sys::statvfs::statvfs("/") {
            Ok(stat) => {
                // The block counts and the block size are `u64` on some
                // systems and narrower on others.
                #[allow(
                    clippy::useless_conversion,
                    reason = "the integer widths differ between Unix systems"
                )]
                let (unit, blocks, available) = (
                    u64::from(stat.fragment_size()),
                    u64::from(stat.blocks()),
                    u64::from(stat.blocks_available()),
                );
                (blocks.checked_mul(unit), available.checked_mul(unit))
            }
            Err(_) => (None, None),
        };
        vec![DriveEntry {
            root: "/".to_owned(),
            // What the root file system lies on is not something this host
            // is asked.
            kind: DriveKind::Unknown,
            label: None,
            total_bytes,
            free_bytes,
        }]
    }
}

#[cfg(not(any(windows, unix)))]
mod sys {
    use cretlink_protocol::{DriveEntry, DriveKind};

    /// The root of a host this crate knows nothing more about.
    pub(super) fn list() -> Vec<DriveEntry> {
        vec![DriveEntry {
            root: "/".to_owned(),
            kind: DriveKind::Unknown,
            label: None,
            total_bytes: None,
            free_bytes: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use cretlink_protocol::{DriveEntry, DriveKind, limits};

    use super::{fit, list};

    fn drive() -> DriveEntry {
        DriveEntry {
            root: "C:\\".to_owned(),
            kind: DriveKind::Fixed,
            label: Some("Windows".to_owned()),
            total_bytes: Some(1000),
            free_bytes: Some(400),
        }
    }

    /// A drive within the bounds is reported as the system gave it.
    #[test]
    fn a_drive_within_the_bounds_is_unchanged() {
        assert_eq!(fit(drive()), Some(drive()));
    }

    /// A label is the drive's own text, written by whoever formatted it: it is
    /// cut to its bound and kept on one line, and an empty one is no label.
    #[test]
    fn a_label_is_bounded_and_kept_on_one_line() {
        let long = fit(DriveEntry {
            label: Some("é".repeat(limits::MAX_DRIVE_LABEL_BYTES)),
            ..drive()
        })
        .unwrap();
        let label = long.label.unwrap();
        assert!(label.len() <= limits::MAX_DRIVE_LABEL_BYTES);
        assert!(label.ends_with('…'), "a cut label says so: {label}");

        let lines = fit(DriveEntry {
            label: Some("Backup\r\nC:\\ (fixed)".to_owned()),
            ..drive()
        })
        .unwrap();
        assert!(!lines.label.unwrap().contains(['\r', '\n']));

        let empty = fit(DriveEntry {
            label: Some(String::new()),
            ..drive()
        })
        .unwrap();
        assert_eq!(empty.label, None);
    }

    /// A size the wire cannot carry, or more free space than the drive holds,
    /// is reported as not known rather than as a wrong number.
    #[test]
    fn a_size_out_of_range_is_not_known() {
        let huge = fit(DriveEntry {
            total_bytes: Some(limits::MAX_FILE_OFFSET + 1),
            free_bytes: Some(5),
            ..drive()
        })
        .unwrap();
        assert_eq!(huge.total_bytes, None);
        assert_eq!(huge.free_bytes, Some(5));

        let inconsistent = fit(DriveEntry {
            total_bytes: Some(1000),
            free_bytes: Some(1001),
            ..drive()
        })
        .unwrap();
        assert_eq!(inconsistent.total_bytes, Some(1000));
        assert_eq!(inconsistent.free_bytes, None);
    }

    /// A drive without a usable root cannot be addressed and is left out.
    #[test]
    fn a_drive_without_a_usable_root_is_left_out() {
        for root in [
            String::new(),
            "C:\\\n".to_owned(),
            "r".repeat(limits::MAX_DRIVE_ROOT_BYTES + 1),
        ] {
            assert_eq!(fit(DriveEntry { root, ..drive() }), None);
        }
    }

    /// The drives of the computer the test runs on: at least one, every one
    /// within the wire bounds, and where both sizes are known the free space
    /// is no more than the size.
    #[test]
    fn this_computer_has_at_least_one_drive_within_the_bounds() {
        let drives = list();
        assert!(!drives.is_empty(), "a computer has at least one drive");
        assert!(drives.len() <= limits::MAX_DRIVES);
        for drive in &drives {
            assert!(!drive.root.is_empty());
            assert!(drive.root.len() <= limits::MAX_DRIVE_ROOT_BYTES);
            if let Some(label) = &drive.label {
                assert!(!label.is_empty());
                assert!(label.len() <= limits::MAX_DRIVE_LABEL_BYTES);
            }
            if let (Some(total), Some(free)) = (drive.total_bytes, drive.free_bytes) {
                assert!(free <= total, "{drive:?}");
            }
        }
    }

    /// Off Windows the one drive is the root file system, with its sizes.
    #[cfg(unix)]
    #[test]
    fn the_root_file_system_is_the_one_drive_off_windows() {
        let drives = list();
        assert_eq!(drives.len(), 1);
        let root = &drives[0];
        assert_eq!(root.root, "/");
        assert_eq!(root.kind, DriveKind::Unknown);
        assert_eq!(root.label, None);
        let total = root.total_bytes.expect("the root file system has a size");
        assert!(total > 0);
        assert!(root.free_bytes.expect("and free space") <= total);
    }

    /// On Windows the drives are drive letters, and the drive the system runs
    /// from is a fixed one with a size.
    #[cfg(windows)]
    #[test]
    fn windows_lists_drive_letters_and_the_system_drive_has_a_size() {
        let drives = list();
        for drive in &drives {
            let bytes = drive.root.as_bytes();
            assert_eq!(bytes.len(), 3, "{drive:?}");
            assert!(bytes[0].is_ascii_uppercase(), "{drive:?}");
            assert_eq!(&bytes[1..], b":\\", "{drive:?}");
            // A drive on the network is listed with its kind only.
            if drive.kind == DriveKind::Network {
                assert_eq!(drive.label, None);
                assert_eq!(drive.total_bytes, None);
                assert_eq!(drive.free_bytes, None);
            }
        }
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        let system = drives
            .iter()
            .find(|drive| drive.root.eq_ignore_ascii_case(&format!("{system}\\")))
            .expect("the system drive is listed");
        assert_eq!(system.kind, DriveKind::Fixed);
        assert!(system.total_bytes.expect("it has a size") > 0);
        assert!(system.free_bytes.is_some());
    }
}
