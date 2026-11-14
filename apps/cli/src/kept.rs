//! The support program that a customer keeps for next time (ST-M009-017,
//! REQ-F-024, ADR-0018).
//!
//! A customer who gets help more than once can keep the support program on
//! the computer and start it from the Start menu the next time. Keeping adds
//! exactly this, and nothing that runs by itself:
//!
//! | What | Where |
//! |---|---|
//! | The program | `%ProgramFiles%\CretLink Support\CretLink-Support.exe` |
//! | The record of what was added | `install.json` next to it |
//! | The relay settings of the copy that was kept, when it had any | `cretlink.toml` next to it, with `relay-ca.pem` when the relay has a certificate file |
//! | A Start-menu entry for everyone who uses the computer | `CretLink Support.lnk` in the Start menu of all users |
//! | On request, a shortcut on the desktop | `CretLink Support.lnk` on the desktop of all users |
//! | An entry in Installed apps | `HKLM\…\Uninstall\CretLink Support` |
//!
//! No service, no scheduled task, no entry that starts with Windows, no
//! listener, no stored permission, and no handler for a protocol or a kind of
//! file: [`System`] has no way to add one. The shortcuts and the entry name
//! the kept program by its full path. A session from the kept program takes
//! every step of a session from a copy that was downloaded.
//!
//! # An elevated program that installs itself (REQ-S-024)
//!
//! The program that keeps itself runs with administrator rights, from a folder
//! that a program without those rights can write. So:
//!
//! - **What is kept is the program that runs.** The file is not copied by its
//!   path: the program file is held open without sharing changes and verified
//!   to be the image of this process ([`System::hold_running_program`],
//!   `cretlink_exec::hold_running_program`), and its bytes are read through
//!   that one handle. What was written is read back and compared before it
//!   takes its name.
//! - **Where it is kept, only administrators can write.** The folder is
//!   created with its owner and its permissions in one step, or found to be
//!   such a folder already ([`System::prepare_folder`]). Every file is
//!   written new under another name and then given its name; something that
//!   is no file under a name of the program is never written through, and a
//!   program that is running is never replaced.
//! - **The relay settings are somebody else's input.** The file next to the
//!   running program is read without following a link, up to a bound, with
//!   the reader of relay profiles; a profile that cannot be used is not kept
//!   and is said. What is kept is written anew from what was read: the
//!   settings of the profile, and the certificates of its certificate file
//!   and nothing else that stood in that file. The same value that the
//!   customer was shown is the one that is written ([`KeepPlan`]).
//! - **Removing removes what the record names**, as far as it lies in the
//!   layout: a file directly in the folder of the kept program, a shortcut
//!   in its own place, the entry under its own key. Nothing is followed
//!   through a link, and a record that names anything else changes nothing
//!   about it.
//! - **Nobody keeps the program but the customer.** There is no command-line
//!   option, no environment variable and no remote operation for it:
//!   [`keep`] is called from the customer's own window only.
//!
//! The customer role exists on Windows only. Elsewhere [`Layout::current`]
//! is `None`, and nothing is offered.

use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cretlink_exec::{Standing, remove_whole};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::install::{
    PUBLISHER, RELAY_CA_FILE, Shortcut, UninstallEntry, same_path, temporary_name,
};
use crate::process;
use crate::relay_profile::{
    self, MAX_CA_BYTES, MAX_PROFILE_BYTES, PROFILE_FILE, Problem, ProfileError, RelayProfile,
};

#[cfg(windows)]
mod win;

/// The file name of the kept program. It starts with `CretLink-Support`, so
/// it opens the customer role and nothing else (ADR-0018).
pub const PROGRAM_FILE: &str = "CretLink-Support.exe";

/// The file name of the record of what was added.
pub const RECORD_FILE: &str = "install.json";

/// The name of the kept program in the Start menu and in Installed apps.
pub const DISPLAY_NAME: &str = "CretLink Support";

/// The file name of the Start-menu entry and of the desktop shortcut.
pub const SHORTCUT_FILE: &str = "CretLink Support.lnk";

/// The key of the entry in Installed apps, under `HKEY_LOCAL_MACHINE`.
pub const UNINSTALL_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\CretLink Support";

/// The largest record that is read (bounded input).
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// The current record format.
const RECORD_SCHEMA: u32 = 1;

/// Where a kept program has each of its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The folder of the kept program, which only administrators can change.
    pub folder: PathBuf,
    /// The kept program.
    pub exe: PathBuf,
    /// The record of what was added.
    pub record: PathBuf,
    /// The relay settings of the kept program, when it has any.
    pub profile: PathBuf,
    /// The certificate file of that relay, when it has one.
    pub relay_ca: PathBuf,
    /// The Start-menu entry, for all users.
    pub start_menu: PathBuf,
    /// The desktop shortcut, for all users.
    pub desktop: PathBuf,
    /// The key of the entry in Installed apps, under `HKEY_LOCAL_MACHINE`.
    pub uninstall_key: String,
}

impl Layout {
    /// The layout of a kept program in `folder`, with its Start-menu entry in
    /// `start_menu` and its desktop shortcut in `desktop`.
    #[must_use]
    pub fn new(folder: PathBuf, start_menu: &Path, desktop: &Path) -> Self {
        Self {
            exe: folder.join(PROGRAM_FILE),
            record: folder.join(RECORD_FILE),
            profile: folder.join(PROFILE_FILE),
            relay_ca: folder.join(RELAY_CA_FILE),
            start_menu: start_menu.join(SHORTCUT_FILE),
            desktop: desktop.join(SHORTCUT_FILE),
            uninstall_key: UNINSTALL_KEY.to_owned(),
            folder,
        }
    }

    /// The layout of this computer, as the system names its folders. `None`
    /// off Windows, where no program is kept, and when the system does not
    /// name a folder: then nothing is offered.
    #[must_use]
    pub fn current() -> Option<Self> {
        #[cfg(windows)]
        {
            win::layout()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Whether `exe` is the kept program, so that this copy runs from where
    /// it is kept.
    #[must_use]
    pub fn is_kept_exe(&self, exe: &Path) -> bool {
        same_path(exe, &self.exe)
    }

    /// The names CretLink gives a file in the folder of the kept program.
    fn own_names() -> [&'static str; 4] {
        [PROGRAM_FILE, PROFILE_FILE, RELAY_CA_FILE, RECORD_FILE]
    }
}

// --- The record ---

/// `install.json`: what keeping the program added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// The record format, currently 1.
    pub schema: u32,
    /// The CretLink release that was kept.
    pub version: String,
    /// The names of the files written into the folder of the kept program.
    pub files: Vec<String>,
    /// The Start-menu entry.
    pub start_menu: Option<PathBuf>,
    /// The desktop shortcut, when the customer asked for one.
    pub desktop: Option<PathBuf>,
    /// The entry in Installed apps.
    pub uninstall_entry: Option<String>,
}

/// Reads the record. `Ok(None)` when there is none.
///
/// # Errors
///
/// Returns a message when something stands under the record's name that
/// cannot be read as one: a link, a file that is too large or malformed.
pub fn read_record(layout: &Layout) -> Result<Option<Record>, String> {
    let bytes = read_untrusted(&layout.record, MAX_RECORD_BYTES).map_err(|err| {
        format!(
            "the record {} cannot be read: {err}",
            layout.record.display()
        )
    })?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| format!("{} is not a valid record: {err}", layout.record.display()))
}

// --- The operating system ---

/// The parts of keeping and removing that are not plain files in the folder
/// of the kept program. It has no way to add anything that runs by itself.
pub trait System {
    /// Fails unless this process acts as an administrator.
    ///
    /// # Errors
    ///
    /// When it does not, or that cannot be told.
    fn require_administrator(&self) -> io::Result<()>;

    /// Makes `folder` the folder of the kept program: created with its owner
    /// and its permissions in one step, or found to be such a folder already
    /// (`cretlink_exec::prepare_program_folder`).
    ///
    /// # Errors
    ///
    /// When something else stands there, or the folder cannot be made.
    fn prepare_folder(&self, folder: &Path) -> io::Result<()>;

    /// Opens the program file of this process and holds it: verified to be
    /// the image this process runs from, and closed to every change for as
    /// long as the file is open (`cretlink_exec::hold_running_program`).
    ///
    /// # Errors
    ///
    /// When the program file cannot be held or is not the image.
    fn hold_running_program(&self) -> io::Result<File>;

    /// Whether a process other than this one runs `exe`.
    ///
    /// # Errors
    ///
    /// Returns an error when the process list cannot be read.
    fn is_running(&self, exe: &Path) -> io::Result<bool>;

    /// Creates or replaces a shortcut.
    ///
    /// # Errors
    ///
    /// Returns an error when the shortcut cannot be written.
    fn create_shortcut(&self, shortcut: &Shortcut) -> io::Result<()>;

    /// Writes the entry in Installed apps under `key`.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be written.
    fn write_uninstall_entry(&self, key: &str, entry: &UninstallEntry) -> io::Result<()>;

    /// Whether the entry under `key` exists.
    fn has_uninstall_entry(&self, key: &str) -> bool;

    /// Removes the entry under `key`; `Ok(false)` when there was none.
    ///
    /// # Errors
    ///
    /// Returns an error when it exists and cannot be removed.
    fn remove_uninstall_entry(&self, key: &str) -> io::Result<bool>;

    /// Removes `files` and then `dir`, if it is empty, after this process has
    /// exited: a running Windows program cannot delete its own file.
    ///
    /// # Errors
    ///
    /// Returns an error when the removal cannot be scheduled.
    fn remove_after_exit(&self, files: &[PathBuf], dir: &Path) -> io::Result<()>;
}

/// The real operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsSystem;

#[cfg(not(windows))]
fn windows_only<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the support program is kept only on Windows",
    ))
}

#[cfg(not(windows))]
impl System for OsSystem {
    fn require_administrator(&self) -> io::Result<()> {
        windows_only()
    }

    fn prepare_folder(&self, _folder: &Path) -> io::Result<()> {
        windows_only()
    }

    fn hold_running_program(&self) -> io::Result<File> {
        windows_only()
    }

    fn is_running(&self, _exe: &Path) -> io::Result<bool> {
        windows_only()
    }

    fn create_shortcut(&self, _shortcut: &Shortcut) -> io::Result<()> {
        windows_only()
    }

    fn write_uninstall_entry(&self, _key: &str, _entry: &UninstallEntry) -> io::Result<()> {
        windows_only()
    }

    fn has_uninstall_entry(&self, _key: &str) -> bool {
        false
    }

    fn remove_uninstall_entry(&self, _key: &str) -> io::Result<bool> {
        windows_only()
    }

    fn remove_after_exit(&self, _files: &[PathBuf], _dir: &Path) -> io::Result<()> {
        windows_only()
    }
}

// --- Files in a folder that is somebody else's, and in our own ---

/// Reads the file at `path` without following a link, up to `limit` bytes.
/// `Ok(None)` when nothing stands there.
///
/// The file may lie in a folder that a program without rights can write. So
/// only a file is read: a link under the name is refused, also one that
/// takes the file's place between the look and the opening, and a file that
/// is larger than the bound is refused before it is read whole.
fn read_untrusted(path: &Path, limit: u64) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let not_a_file = || io::Error::new(io::ErrorKind::InvalidInput, "it is not a file");
    let looked = match fs::symlink_metadata(path) {
        Ok(facts) => facts,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !looked.file_type().is_file() {
        return Err(not_a_file());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // What stands under the name is opened itself, never what a link
        // there points at.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    // What was opened is what was looked at: a file, and the same one.
    let opened = file.metadata()?;
    if !opened.file_type().is_file() || !same_entry(&looked, &opened) {
        return Err(not_a_file());
    }
    if opened.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("it is larger than {} KiB", limit / 1024),
        ));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    // One byte more than the bound shows a file that grew since.
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("it is larger than {} KiB", limit / 1024),
        ));
    }
    Ok(Some(bytes))
}

/// Whether the entry that was looked at by its name and the one that was
/// opened are the same. Where the system tells entries apart by a number
/// (Unix), that number decides; on Windows the open itself refuses to follow
/// a link.
fn same_entry(looked: &fs::Metadata, opened: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        looked.dev() == opened.dev() && looked.ino() == opened.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (looked, opened);
        true
    }
}

/// Removes the file or the link at `path`. `Ok(false)` when nothing stands
/// there. A folder under the name is not CretLink's and is an error.
fn remove_own_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
        Ok(facts) if facts.is_dir() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a folder stands under its name",
        )),
        // A file, or a link, which is removed as a link.
        Ok(_) => remove_whole(path).map(|()| true),
    }
}

/// Writes a file of the kept program: new, under another name beside its
/// place, and given its name only when `write` and `check` succeeded. What
/// stands under the name is replaced when it is a file; anything else there
/// is an error, and nothing is written through it.
fn write_new(
    target: &Path,
    write: &mut dyn FnMut(&mut File) -> io::Result<()>,
    check: &mut dyn FnMut(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    if Standing::of_file(target) == Standing::Foreign {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "something that is no file stands under its name",
        ));
    }
    let temporary = temporary_name(target);
    // What an interrupted run left under the temporary name.
    remove_own_file(&temporary)?;
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        write(&mut file)?;
        drop(file);
        check(&mut File::open(&temporary)?)?;
        fs::rename(&temporary, target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Writes `bytes` as the file `target` of the kept program ([`write_new`]).
fn write_bytes(target: &Path, bytes: &[u8]) -> io::Result<()> {
    write_new(target, &mut |file| file.write_all(bytes), &mut |_| Ok(()))
}

/// Writes the held program `source` as `target`, and reads what was written
/// back against it before it takes its name.
fn write_program(source: &File, target: &Path) -> io::Result<()> {
    write_new(
        target,
        &mut |file| {
            let mut source = source;
            source.seek(SeekFrom::Start(0))?;
            io::copy(&mut source, file).map(drop)
        },
        &mut |written| {
            let mut source = source;
            source.seek(SeekFrom::Start(0))?;
            if same_bytes(&mut source, written)? {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "what was written is not the program that runs",
                ))
            }
        },
    )
}

/// Whether two readers give the same bytes to their ends.
fn same_bytes(one: &mut dyn Read, other: &mut dyn Read) -> io::Result<bool> {
    let mut a = vec![0u8; 64 * 1024];
    let mut b = vec![0u8; 64 * 1024];
    loop {
        let read = read_full(one, &mut a)?;
        if read_full(other, &mut b)? != read || a[..read] != b[..read] {
            return Ok(false);
        }
        if read == 0 {
            return Ok(true);
        }
    }
}

/// Fills `buffer` as far as the reader goes, and answers how far that was.
fn read_full(reader: &mut dyn Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

// --- The relay settings that come with the program ---

/// The relay settings of the copy that runs, read and checked, as they are
/// kept: the profile, naming its certificate file as the kept program has
/// it, and the certificates of that file written anew.
#[derive(Debug, Clone)]
pub struct RelayToKeep {
    profile: RelayProfile,
    certificates: Option<Vec<u8>>,
}

impl RelayToKeep {
    /// The profile as it is kept. It holds the admission key of the copy.
    #[must_use]
    pub fn profile(&self) -> &RelayProfile {
        &self.profile
    }

    /// Whether the relay's certificate is checked against a certificate file
    /// of the profile, and not against the system's authorities.
    #[must_use]
    pub fn has_certificate_file(&self) -> bool {
        self.certificates.is_some()
    }

    /// The text of the kept `cretlink.toml`. It holds the key in readable
    /// form and is erased when dropped.
    fn text(&self) -> Zeroizing<String> {
        let mut text = Zeroizing::new(String::from(
            "# The relay of the CretLink support program that is kept on this computer.\n\
             # Written by CretLink when the program was kept.\n\
             # A key in it stands in readable form. It only lets this copy use the relay.\n",
        ));
        text.push_str(&self.profile.to_section());
        text
    }
}

/// Reads the relay settings next to the program `exe`, as somebody else's
/// input: without following a link, up to a bound, with the reader of relay
/// profiles. `Ok(None)` when the copy has no relay settings.
///
/// # Errors
///
/// Returns the file and what is wrong with it, in words that hold nothing of
/// what the file held. Such settings are not kept.
pub fn read_relay_next_to(exe: &Path) -> Result<Option<RelayToKeep>, ProfileError> {
    let Some(file) = relay_profile::path_next_to(exe) else {
        return Ok(None);
    };
    let in_file = |problem: Problem| ProfileError {
        file: file.clone(),
        problem,
    };
    let unreadable = |what: &str, err: &io::Error| {
        // The system's own message may name paths: only the kind is said.
        Problem::file(format!("{what} could not be read ({})", err.kind()))
    };
    let Some(bytes) = read_untrusted(&file, MAX_PROFILE_BYTES)
        .map_err(|err| in_file(unreadable("the file", &err)))?
    else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| in_file(Problem::file("the file is not text (UTF-8)")))?;
    let Some(profile) = relay_profile::parse(text).map_err(in_file)? else {
        return Ok(None);
    };
    let Some(name) = profile.ca().map(str::to_owned) else {
        return Ok(Some(RelayToKeep {
            profile,
            certificates: None,
        }));
    };
    // The reader took `ca` only as one plain name, so this is a file in the
    // folder of the profile.
    let ca = file.with_file_name(name);
    let whole = read_untrusted(&ca, MAX_CA_BYTES)
        .map_err(|err| in_file(unreadable("the file that `ca` names", &err)))?
        .ok_or_else(|| {
            in_file(Problem::file(
                "`ca` names a file that is not in the folder of this file",
            ))
        })?;
    // The certificates only: a key that stood beside them is not kept.
    let certificates = relay_profile::certificates_only(&whole).map_err(in_file)?;
    let profile = profile.with_ca(Some(RELAY_CA_FILE)).map_err(in_file)?;
    Ok(Some(RelayToKeep {
        profile,
        certificates: Some(certificates),
    }))
}

// --- Versions ---

/// The three numbers of a version and what follows them after a hyphen.
fn parse_version(text: &str) -> Option<([u64; 3], Option<&str>)> {
    let text = text.split('+').next()?;
    let (numbers, rest) = match text.split_once('-') {
        Some((numbers, rest)) => (numbers, Some(rest)),
        None => (text, None),
    };
    let mut parts = numbers.split('.').map(str::parse::<u64>);
    let triple = [
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    ];
    parts.next().is_none().then_some((triple, rest))
}

/// How the version `this` compares to `kept`. `None` when that cannot be
/// told: one of them is not a version, or both are different previews of the
/// same release.
#[must_use]
pub fn compare_versions(this: &str, kept: &str) -> Option<Ordering> {
    let (this_numbers, this_preview) = parse_version(this)?;
    let (kept_numbers, kept_preview) = parse_version(kept)?;
    match this_numbers.cmp(&kept_numbers) {
        Ordering::Equal => match (this_preview, kept_preview) {
            (None, None) => Some(Ordering::Equal),
            // A release is newer than a preview of it.
            (None, Some(_)) => Some(Ordering::Greater),
            (Some(_), None) => Some(Ordering::Less),
            (Some(this), Some(kept)) if this == kept => Some(Ordering::Equal),
            (Some(_), Some(_)) => None,
        },
        other => Some(other),
    }
}

/// How the copy that runs relates to the program that is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// No program is kept on this computer.
    NotKept,
    /// The copy that runs is the kept program.
    ThisIsIt,
    /// The kept program has the version of the copy that runs.
    Same,
    /// The copy that runs is newer: it can update the kept program.
    ThisIsNewer,
    /// The kept program is newer: it is never replaced by this copy.
    ThisIsOlder,
    /// A program is kept and its version cannot be compared with this one:
    /// it is not replaced.
    Unknown,
}

/// What is kept on this computer, as the copy `this_exe` of the version
/// `this_version` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// How this copy relates to what is kept.
    pub relation: Relation,
    /// The version of the kept program, as its record says. `None` when
    /// nothing is kept or the record cannot be read.
    pub kept_version: Option<String>,
}

/// Looks at what is kept. It changes nothing and needs no administrator
/// rights.
#[must_use]
pub fn status(layout: &Layout, this_exe: &Path, this_version: &str) -> Status {
    if Standing::of_file(&layout.exe) != Standing::Ours {
        return Status {
            relation: Relation::NotKept,
            kept_version: None,
        };
    }
    let kept_version = read_record(layout)
        .ok()
        .flatten()
        .map(|record| record.version);
    let relation = if layout.is_kept_exe(this_exe) {
        Relation::ThisIsIt
    } else {
        match kept_version
            .as_deref()
            .and_then(|kept| compare_versions(this_version, kept))
        {
            Some(Ordering::Equal) => Relation::Same,
            Some(Ordering::Greater) => Relation::ThisIsNewer,
            Some(Ordering::Less) => Relation::ThisIsOlder,
            None => Relation::Unknown,
        }
    };
    Status {
        relation,
        kept_version,
    }
}

/// The program that is kept on this computer: its folder, and its version
/// when its record says one. `None` when nothing is kept.
#[must_use]
pub fn kept(layout: &Layout) -> Option<(PathBuf, Option<String>)> {
    if Standing::of_file(&layout.exe) != Standing::Ours {
        return None;
    }
    let version = read_record(layout)
        .ok()
        .flatten()
        .map(|record| record.version);
    Some((layout.folder.clone(), version))
}

/// What a report says about a program that is kept on this computer, so that
/// nobody takes it for something a session left behind (REQ-F-019): that it
/// is there because it was kept, and how it goes again. `None` when nothing
/// is kept.
#[must_use]
pub fn on_purpose(layout: &Layout) -> Option<String> {
    let (folder, version) = kept(layout)?;
    let version = version
        .map(|version| format!(", version {version}"))
        .unwrap_or_default();
    Some(format!(
        "CretLink Support is installed on this computer ({}{version}) because it was kept there \
         on purpose. To remove it, use Installed apps in the settings of Windows, or run \
         '{PROGRAM_FILE} uninstall'.",
        folder.display()
    ))
}

// --- Keeping ---

/// Why the program was not kept, or not removed.
#[derive(Debug)]
pub enum KeptError {
    /// This process does not run with administrator rights.
    NotAdministrator,
    /// The copy that runs is the kept program already.
    AlreadyKept,
    /// The kept program has the version of this copy.
    SameVersion(String),
    /// The kept program is newer than this copy, or its version cannot be
    /// compared: it is not replaced.
    NotNewer {
        /// The version that is kept, when its record says one.
        kept: Option<String>,
        /// The version of this copy.
        this: String,
    },
    /// The kept program runs in another process.
    Running(PathBuf),
    /// No program is kept, and nothing of one is there.
    NotKept(PathBuf),
    /// The file of the program that runs could not be held unchanged, or is
    /// not the image this process runs from: nothing is kept from a file
    /// that could be another one.
    Program(io::Error),
    /// A file or system step failed.
    Io {
        /// What was being done, as "could not \<stage\>".
        stage: &'static str,
        /// The path concerned.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
}

impl KeptError {
    fn io(stage: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            stage,
            path: path.to_path_buf(),
            source,
        }
    }

    /// A short name of the failure, for a screen that words it itself.
    #[must_use]
    pub const fn stage(&self) -> &'static str {
        match self {
            Self::NotAdministrator => "keep_administrator",
            Self::AlreadyKept | Self::SameVersion(_) => "keep_already",
            Self::NotNewer { .. } => "keep_older",
            Self::Running(_) => "keep_running",
            Self::NotKept(_) => "keep_absent",
            Self::Program(_) => "keep_program",
            Self::Io { .. } => "keep_failed",
        }
    }
}

impl std::fmt::Display for KeptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAdministrator => {
                f.write_str("this needs administrator rights, and this window runs without them")
            }
            Self::AlreadyKept => {
                f.write_str("this is the support program that is kept on this computer")
            }
            Self::SameVersion(version) => write!(
                f,
                "CretLink Support is kept on this computer in this version already ({version})"
            ),
            Self::NotNewer {
                kept: Some(kept),
                this,
            } => write!(
                f,
                "the support program that is kept on this computer ({kept}) is not older than \
                 this copy ({this}); it is not replaced"
            ),
            Self::NotNewer { kept: None, this } => write!(
                f,
                "a support program is kept on this computer whose version cannot be read; it is \
                 not replaced by this copy ({this})"
            ),
            Self::Running(path) => write!(
                f,
                "the kept support program is running ({}). Close it, then try again",
                path.display()
            ),
            Self::NotKept(folder) => write!(
                f,
                "CretLink Support is not kept on this computer (nothing of it in {})",
                folder.display()
            ),
            Self::Program(source) => write!(
                f,
                "the file of the program that runs could not be held as it is ({source}); \
                 nothing was kept, because what would be kept could be another file. Close \
                 other programs that use this copy, then try again"
            ),
            Self::Io {
                stage,
                path,
                source,
            } => write!(f, "could not {stage} ({}): {source}", path.display()),
        }
    }
}

impl std::error::Error for KeptError {}

/// What keeping would do, as the customer is shown it before choosing, and
/// what [`keep`] then does: the relay settings in it are the ones that were
/// read for the screen, and no others are written.
#[derive(Debug, Clone)]
pub struct KeepPlan {
    /// The version of the copy that runs, which would be kept.
    pub version: String,
    /// The version it would replace, when a program is kept already.
    pub replaces: Option<String>,
    /// Whether the kept program has a desktop shortcut already.
    pub desktop_kept: bool,
    /// The relay settings of this copy, which would be kept.
    pub relay: Option<RelayToKeep>,
    /// Why the relay settings next to this copy are not kept, when they
    /// cannot be used.
    pub relay_problem: Option<ProfileError>,
    /// Whether relay settings that are kept already would be replaced.
    pub replaces_relay: bool,
}

/// Plans keeping the copy `this_exe` of the version `this_version`. It
/// changes nothing.
///
/// # Errors
///
/// Refuses a process without administrator rights, the kept program itself,
/// and a copy that is not newer than what is kept.
pub fn keep_plan(
    layout: &Layout,
    system: &dyn System,
    this_exe: &Path,
    this_version: &str,
) -> Result<KeepPlan, KeptError> {
    system
        .require_administrator()
        .map_err(|_| KeptError::NotAdministrator)?;
    let status = status(layout, this_exe, this_version);
    match status.relation {
        Relation::NotKept | Relation::ThisIsNewer => {}
        Relation::ThisIsIt => return Err(KeptError::AlreadyKept),
        Relation::Same => return Err(KeptError::SameVersion(this_version.to_owned())),
        Relation::ThisIsOlder | Relation::Unknown => {
            return Err(KeptError::NotNewer {
                kept: status.kept_version,
                this: this_version.to_owned(),
            });
        }
    }
    let (relay, relay_problem) = match read_relay_next_to(this_exe) {
        Ok(relay) => (relay, None),
        Err(problem) => (None, Some(problem)),
    };
    let record = read_record(layout).ok().flatten();
    Ok(KeepPlan {
        version: this_version.to_owned(),
        replaces: (status.relation == Relation::ThisIsNewer)
            .then_some(status.kept_version)
            .flatten(),
        desktop_kept: record.is_some_and(|record| record.desktop.is_some()),
        replaces_relay: relay.is_some() && Standing::of_file(&layout.profile) != Standing::Absent,
        relay,
        relay_problem,
    })
}

/// What keeping did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepReport {
    /// The kept program.
    pub exe: PathBuf,
    /// The version that was kept.
    pub version: String,
    /// The version it replaced, when a program was kept already.
    pub replaced: Option<String>,
    /// Whether relay settings were kept with it.
    pub relay: bool,
    /// Whether there is a desktop shortcut.
    pub desktop: bool,
}

/// Keeps the program that runs, as `plan` says: writes it to the folder that
/// only administrators can change, with the relay settings of the plan, adds
/// the Start-menu entry, the desktop shortcut when `desktop` asks for one,
/// and the entry in Installed apps, and records all of it.
///
/// # Errors
///
/// Stops before changing anything for a process without administrator
/// rights and when the kept program runs; stops on the first step that
/// fails. What was added until then is in the record and is removed by
/// [`remove`].
pub fn keep(
    layout: &Layout,
    system: &dyn System,
    plan: &KeepPlan,
    desktop: bool,
) -> Result<KeepReport, KeptError> {
    system
        .require_administrator()
        .map_err(|_| KeptError::NotAdministrator)?;
    let running = system.is_running(&layout.exe).map_err(|err| {
        KeptError::io(
            "check whether the kept program is running",
            &layout.exe,
            err,
        )
    })?;
    if running {
        return Err(KeptError::Running(layout.exe.clone()));
    }
    // The program that runs is held before anything is made: verified to be
    // the image of this process, and closed to every change until it is
    // written. When it cannot be held, nothing was added.
    let source = system.hold_running_program().map_err(KeptError::Program)?;
    system
        .prepare_folder(&layout.folder)
        .map_err(|err| KeptError::io("make the folder of the kept program", &layout.folder, err))?;

    // The record first, with everything this run is going to add: whatever
    // then exists is named by a record, also when the run is interrupted.
    let previous = read_record(layout).ok().flatten();
    let mut files = vec![PROGRAM_FILE.to_owned(), RECORD_FILE.to_owned()];
    let keeps_relay = plan.relay.is_some()
        || previous
            .as_ref()
            .is_some_and(|record| record.files.iter().any(|name| name == PROFILE_FILE));
    if keeps_relay {
        files.push(PROFILE_FILE.to_owned());
        files.push(RELAY_CA_FILE.to_owned());
    }
    let record = Record {
        schema: RECORD_SCHEMA,
        version: plan.version.clone(),
        files,
        start_menu: Some(layout.start_menu.clone()),
        desktop: (desktop
            || previous
                .as_ref()
                .is_some_and(|record| record.desktop.is_some()))
        .then(|| layout.desktop.clone()),
        uninstall_entry: Some(format!(r"HKLM\{}", layout.uninstall_key)),
    };
    write_record(layout, &record)?;

    // Read through the held file, never by a path.
    write_program(&source, &layout.exe)
        .map_err(|err| KeptError::io("write the program", &layout.exe, err))?;
    drop(source);

    if let Some(relay) = &plan.relay {
        match &relay.certificates {
            Some(certificates) => write_bytes(&layout.relay_ca, certificates),
            // The certificate file of the settings that are replaced.
            None => remove_own_file(&layout.relay_ca).map(drop),
        }
        .map_err(|err| KeptError::io("write the certificate file", &layout.relay_ca, err))?;
        write_bytes(&layout.profile, relay.text().as_bytes())
            .map_err(|err| KeptError::io("write the relay settings", &layout.profile, err))?;
    }

    let exe = layout.exe.to_string_lossy().into_owned();
    let shortcut = |path: &Path| Shortcut {
        path: path.to_path_buf(),
        target: layout.exe.clone(),
        arguments: String::new(),
        description: "CretLink Support: get help".to_owned(),
    };
    add_shortcut(
        system,
        &shortcut(&layout.start_menu),
        "add the Start-menu entry",
    )?;
    if desktop {
        add_shortcut(
            system,
            &shortcut(&layout.desktop),
            "add the desktop shortcut",
        )?;
    } else if previous.is_some_and(|record| record.desktop.is_some()) {
        // Not wanted any more: the one an earlier run added goes.
        remove_own_file(&layout.desktop)
            .map_err(|err| KeptError::io("remove the desktop shortcut", &layout.desktop, err))?;
    }
    system
        .write_uninstall_entry(
            &layout.uninstall_key,
            &UninstallEntry {
                display_name: DISPLAY_NAME.to_owned(),
                display_version: plan.version.clone(),
                publisher: PUBLISHER.to_owned(),
                install_location: layout.folder.to_string_lossy().into_owned(),
                uninstall_string: format!("\"{exe}\" uninstall"),
                display_icon: exe,
            },
        )
        .map_err(|err| {
            KeptError::io(
                "add the entry in Installed apps",
                Path::new(&layout.uninstall_key),
                err,
            )
        })?;

    // The record as it is now: without a desktop shortcut that went.
    let record = Record {
        desktop: desktop.then(|| layout.desktop.clone()),
        ..record
    };
    write_record(layout, &record)?;
    Ok(KeepReport {
        exe: layout.exe.clone(),
        version: plan.version.clone(),
        replaced: plan.replaces.clone(),
        relay: Standing::of_file(&layout.profile) == Standing::Ours,
        desktop,
    })
}

fn write_record(layout: &Layout, record: &Record) -> Result<(), KeptError> {
    let failed = |err| KeptError::io("write the record", &layout.record, err);
    let text = serde_json::to_string_pretty(record).map_err(|err| failed(io::Error::other(err)))?;
    write_bytes(&layout.record, text.as_bytes()).map_err(failed)
}

/// Adds a shortcut where nothing but a file of that name may stand: a link
/// or a folder under its name is never written through.
fn add_shortcut(
    system: &dyn System,
    shortcut: &Shortcut,
    stage: &'static str,
) -> Result<(), KeptError> {
    if Standing::of_file(&shortcut.path) == Standing::Foreign {
        return Err(KeptError::io(
            stage,
            &shortcut.path,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "something that is no file stands under its name",
            ),
        ));
    }
    system
        .create_shortcut(shortcut)
        .map_err(|err| KeptError::io(stage, &shortcut.path, err))
}

// --- Removing ---

/// What removing will remove, shown before anything changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovePlan {
    /// The folder of the kept program.
    pub folder: PathBuf,
    /// The files in it that are removed.
    pub files: Vec<PathBuf>,
    /// The shortcuts that are removed.
    pub shortcuts: Vec<PathBuf>,
    /// The key of the entry in Installed apps, when it is there.
    pub uninstall_entry: Option<String>,
    /// Whether the folder is removed only after this process has exited: the
    /// removal runs from the kept program itself.
    pub after_exit: bool,
    /// What is left where it is, in words: what the record names outside the
    /// layout, and what CretLink did not put into the folder.
    pub left: Vec<String>,
}

impl RemovePlan {
    /// What will be removed, one line each, for display.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let after = if self.after_exit {
            " (removed once CretLink has closed)"
        } else {
            ""
        };
        let mut lines: Vec<String> = self
            .files
            .iter()
            .map(|file| format!("{}{after}", file.display()))
            .collect();
        if !self.files.is_empty() {
            lines.push(format!("The folder {}{after}", self.folder.display()));
        }
        for shortcut in &self.shortcuts {
            lines.push(format!("The shortcut {}", shortcut.display()));
        }
        if let Some(key) = &self.uninstall_entry {
            lines.push(format!(r"The entry in Installed apps (HKLM\{key})"));
        }
        lines
    }
}

/// Whether `name` is one plain file name: what the record may name in the
/// folder of the kept program.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
}

/// Plans removing the kept program. It changes nothing and needs no
/// administrator rights.
///
/// What is removed is what the record names, as far as it lies in the
/// layout: a file directly in the folder of the kept program, the shortcuts
/// in their own places, the entry under its own key. Without a record that
/// can be read, it is what CretLink itself names in that folder. Everything
/// else is left, and said.
///
/// # Errors
///
/// [`KeptError::NotKept`] when nothing of a kept program is there.
pub fn remove_plan(
    layout: &Layout,
    system: &dyn System,
    current_exe: &Path,
) -> Result<RemovePlan, KeptError> {
    let mut left = Vec::new();
    // A link in the place of the folder is not the folder CretLink made:
    // nothing behind it is looked at or removed.
    let folder_is_ours = match Standing::of_dir(&layout.folder) {
        Standing::Ours => true,
        Standing::Absent => false,
        Standing::Foreign => {
            left.push(format!(
                "{}, which is not the folder that CretLink made: something else stands under \
                 its name",
                layout.folder.display()
            ));
            false
        }
    };
    let record = if folder_is_ours {
        match read_record(layout) {
            Ok(record) => record,
            Err(reason) => {
                left.push(format!("What only the record could name, because {reason}"));
                None
            }
        }
    } else {
        None
    };

    // The files: the record's names, as far as each is one plain name, and
    // CretLink's own names when no record can be read.
    let mut names: Vec<String> = Vec::new();
    match &record {
        Some(record) => {
            for name in &record.files {
                if is_plain_name(name) {
                    names.push(name.clone());
                } else {
                    left.push(format!(
                        "An entry of the record that does not name a file in {}",
                        layout.folder.display()
                    ));
                }
            }
        }
        None => names.extend(Layout::own_names().map(str::to_owned)),
    }
    // The record itself goes last, and what an interrupted run left under a
    // temporary name goes with its file.
    names.retain(|name| name != RECORD_FILE);
    names.push(RECORD_FILE.to_owned());
    let mut files: Vec<PathBuf> = Vec::new();
    if folder_is_ours {
        for name in &names {
            let file = layout.folder.join(name);
            for path in [temporary_name(&file), file] {
                if fs::symlink_metadata(&path).is_ok() && !files.contains(&path) {
                    files.push(path);
                }
            }
        }
        // What lies in the folder and is not CretLink's keeps the folder.
        if let Ok(entries) = fs::read_dir(&layout.folder) {
            let foreign = entries
                .flatten()
                .filter(|entry| !files.contains(&entry.path()))
                .count();
            if foreign > 0 {
                left.push(format!(
                    "The folder {} with {foreign} {} in it that CretLink did not put there",
                    layout.folder.display(),
                    if foreign == 1 { "entry" } else { "entries" }
                ));
            }
        }
    }

    // The shortcuts: only in their own places. The record says which were
    // added; without one, both places are looked at.
    let recorded = |named: Option<&PathBuf>, own: &PathBuf| match &record {
        Some(_) => named.is_some_and(|named| named == own),
        None => true,
    };
    let mut shortcuts = Vec::new();
    for (named, own) in [
        (
            record.as_ref().and_then(|r| r.start_menu.as_ref()),
            &layout.start_menu,
        ),
        (
            record.as_ref().and_then(|r| r.desktop.as_ref()),
            &layout.desktop,
        ),
    ] {
        if recorded(named, own) && fs::symlink_metadata(own).is_ok() {
            shortcuts.push(own.clone());
        } else if named.is_some_and(|named| named != own) {
            left.push("A shortcut that the record names outside its own place".to_owned());
        }
    }

    let uninstall_entry = system
        .has_uninstall_entry(&layout.uninstall_key)
        .then(|| layout.uninstall_key.clone());
    if files.is_empty() && shortcuts.is_empty() && uninstall_entry.is_none() {
        return Err(KeptError::NotKept(layout.folder.clone()));
    }
    Ok(RemovePlan {
        folder: layout.folder.clone(),
        after_exit: layout.is_kept_exe(current_exe),
        files,
        shortcuts,
        uninstall_entry,
        left,
    })
}

/// What removing did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveReport {
    /// What was removed, in words.
    pub removed: Vec<String>,
    /// What is removed once this process has exited.
    pub pending: Vec<String>,
    /// What stays on purpose, in words.
    pub left: Vec<String>,
    /// What could not be removed, in words. Empty when all of it went.
    pub failed: Vec<String>,
}

impl RemoveReport {
    /// Whether everything planned was removed or scheduled.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Carries out `plan`. Every step is tried; what failed is reported, never
/// hidden. Each file is removed as what stands under its name: a link is
/// removed as a link and never followed.
///
/// # Errors
///
/// Stops before changing anything for a process without administrator
/// rights and when the kept program runs in another process.
pub fn remove(
    layout: &Layout,
    system: &dyn System,
    plan: &RemovePlan,
) -> Result<RemoveReport, KeptError> {
    system
        .require_administrator()
        .map_err(|_| KeptError::NotAdministrator)?;
    let running = system.is_running(&layout.exe).map_err(|err| {
        KeptError::io(
            "check whether the kept program is running",
            &layout.exe,
            err,
        )
    })?;
    if running {
        return Err(KeptError::Running(layout.exe.clone()));
    }
    let mut report = RemoveReport {
        left: plan.left.clone(),
        ..RemoveReport::default()
    };

    for shortcut in &plan.shortcuts {
        match remove_own_file(shortcut) {
            Ok(_) => report
                .removed
                .push(format!("The shortcut {}", shortcut.display())),
            Err(err) => report
                .failed
                .push(format!("The shortcut {}: {err}", shortcut.display())),
        }
    }
    if let Some(key) = &plan.uninstall_entry {
        match system.remove_uninstall_entry(key) {
            Ok(_) => report
                .removed
                .push("The entry in Installed apps".to_owned()),
            Err(err) => report
                .failed
                .push(format!(r"The entry in Installed apps (HKLM\{key}): {err}")),
        }
    }

    // Only files directly in the folder that CretLink made: the plan holds
    // no others, and this is checked again where it matters.
    let files: Vec<&PathBuf> = plan
        .files
        .iter()
        .filter(|file| file.parent() == Some(layout.folder.as_path()))
        .collect();
    if Standing::of_dir(&layout.folder) != Standing::Ours {
        return Ok(report);
    }
    if plan.after_exit {
        let files: Vec<PathBuf> = files.into_iter().cloned().collect();
        match system.remove_after_exit(&files, &layout.folder) {
            Ok(()) => report.pending.push(format!(
                "The folder {}, once CretLink has closed",
                layout.folder.display()
            )),
            Err(err) => report
                .failed
                .push(format!("The folder {}: {err}", layout.folder.display())),
        }
    } else {
        for file in files {
            match remove_own_file(file) {
                Ok(true) => report.removed.push(file.display().to_string()),
                Ok(false) => {}
                Err(err) => report.failed.push(format!("{}: {err}", file.display())),
            }
        }
        // Removed only when empty: what CretLink did not put there keeps it.
        if fs::remove_dir(&layout.folder).is_ok() {
            report
                .removed
                .push(format!("The folder {}", layout.folder.display()));
        }
    }
    Ok(report)
}

// --- `CretLink-Support.exe uninstall` ---

/// What removing will remove and what stays, before anything changes.
fn remove_plan_text(plan: &RemovePlan) -> String {
    let mut text = format!(
        "Remove CretLink Support from this computer ({}).\nThis removes:\n",
        plan.folder.display()
    );
    for line in plan.lines() {
        text.push_str(&format!("  - {line}\n"));
    }
    text.push_str("This stays:\n");
    for line in &plan.left {
        text.push_str(&format!("  - {line}\n"));
    }
    text.push_str("  - A session report that you saved, where you saved it\n");
    text
}

/// What removing did, failures first.
fn remove_report_text(report: &RemoveReport) -> String {
    let mut text = String::new();
    let mut section = |title: &str, lines: &[String]| {
        if lines.is_empty() {
            return;
        }
        text.push_str(title);
        text.push('\n');
        for line in lines {
            text.push_str(&format!("  - {line}\n"));
        }
    };
    section("Could not remove:", &report.failed);
    section("Removed:", &report.removed);
    section("Removed once CretLink has closed:", &report.pending);
    section("Left in place:", &report.left);
    if report.complete() {
        text.push_str("CretLink Support was removed from this computer.\n");
    } else {
        text.push_str(
            "CretLink Support was not completely removed; remove what is listed under 'Could \
             not remove' by hand, as an administrator, or run this again.\n",
        );
    }
    text
}

/// `CretLink-Support.exe uninstall`, also what the entry in Installed apps
/// runs: shows what it will remove, asks, then removes the kept program
/// (ST-M009-017). A process without administrator rights starts itself again
/// through the elevation prompt of Windows and does nothing else; the
/// elevated one shows the list and asks. `in_box` tells that there is no
/// terminal to ask in: a program without a console asks in a message box.
pub fn uninstall_command(program: &str, yes: bool, in_box: bool) -> ExitCode {
    let title = "Remove CretLink Support";
    let tell = |text: &str, failed: bool| {
        if in_box {
            if failed {
                process::alert(title, text);
            } else {
                process::inform(title, text);
            }
        } else if failed {
            eprintln!("{program}: {}", text.trim_end());
        } else {
            println!("{}", text.trim_end());
        }
    };
    let Some(layout) = Layout::current() else {
        tell(
            "The support program is kept only on a Windows computer; nothing to do.",
            false,
        );
        return ExitCode::SUCCESS;
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            tell(
                &format!("cannot determine the path of this program: {err}"),
                true,
            );
            return ExitCode::FAILURE;
        }
    };
    let system = OsSystem;
    let plan = match remove_plan(&layout, &system, &exe) {
        Ok(plan) => plan,
        Err(err @ KeptError::NotKept(_)) => {
            tell(&format!("{err}; nothing to do."), false);
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            tell(&err.to_string(), true);
            return ExitCode::FAILURE;
        }
    };
    if system.require_administrator().is_err() {
        // Windows asks; the elevated program shows the list and asks itself.
        let mut args = vec!["uninstall".to_owned()];
        if yes {
            args.push("--yes".to_owned());
        }
        return match process::relaunch_elevated(&exe, &args) {
            Ok(process::Elevation::Started) => ExitCode::SUCCESS,
            Ok(process::Elevation::Declined) => {
                tell(
                    "Windows was not given permission; nothing was removed.",
                    false,
                );
                ExitCode::FAILURE
            }
            Err(err) => {
                tell(
                    &format!(
                        "Removing needs administrator rights, and Windows could not be asked \
                         for them: {err}. Nothing was removed."
                    ),
                    true,
                );
                ExitCode::FAILURE
            }
        };
    }
    let text = remove_plan_text(&plan);
    let confirmed = if yes {
        if !in_box {
            print!("{text}");
        }
        true
    } else if in_box {
        process::confirm(title, &format!("{text}\nRemove now?"))
    } else {
        print!("{text}Remove now? [y/N] ");
        let _ = io::stdout().flush();
        crate::confirmed(&mut io::stdin().lock())
    };
    if !confirmed {
        tell("Nothing was removed.", false);
        return ExitCode::FAILURE;
    }
    match remove(&layout, &system, &plan) {
        Ok(report) => {
            let complete = report.complete();
            tell(&remove_report_text(&report), !complete);
            if complete {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            tell(&format!("{err}. Nothing was removed."), true);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;
