//! Run once, or install the technician role for this user (ST-M009-006,
//! REQ-F-024, ADR-0018).
//!
//! `cretlink` runs from wherever it was unpacked: that is a *run-once* copy,
//! which writes nothing but the audit trail. *Install* copies the program to a
//! location that is fixed per operating system, so the AI-client registrations
//! made against it keep working after an update in place:
//!
//! | OS      | Location                                   | Menu entry                                   |
//! |---------|--------------------------------------------|----------------------------------------------|
//! | Windows | `%LOCALAPPDATA%\Programs\CretLink\`        | Start-menu shortcut, HKCU uninstall entry    |
//! | macOS   | `~/Applications/CretLink.app` (generated)  | none (the bundle itself)                     |
//! | Linux   | `~/.local/bin/`                            | `~/.local/share/applications/cretlink.desktop` |
//!
//! Install needs no administrator rights and creates no service and no
//! auto-start. It writes `install.json` (the [`InstallRecord`]) with what it
//! created, including the client registrations it made; uninstall reads it to
//! remove exactly that, and removes a recorded registration only while it still
//! points at the installed program. The audit trail is never removed.
//!
//! On Linux the record lives in `~/.local/share/CretLink/install.json` rather
//! than in `~/.local/bin`, which is a shared `PATH` directory; on macOS it is
//! inside the bundle, `Contents/Resources/install.json`.
//!
//! The settings of the installed copy — its relay and the network it waits on
//! (ST-M007-011, [`crate::settings`]) — are kept next to the record, and
//! uninstall removes them with it. A relay profile `cretlink.toml` next to the
//! installed program is not part of the install: uninstall leaves it, and the
//! plan says that it stays, where it lies and whether it holds a key.
//!
//! Everything that touches the operating system beyond plain files — the
//! Windows shortcut and registry entry, the process list, removing a running
//! program after it exits — goes through [`System`], so the logic is tested
//! over a temporary home directory with a fake.
//!
//! Uninstall deletes only the paths of the [`Layout`], never a path read from
//! the record: a tampered record cannot make it delete anything else.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::relay_profile;
use crate::setup::{self, Action, Client, RegistrationState, Runner};

#[cfg(windows)]
mod win;

/// The record's file name.
pub const RECORD_FILE: &str = "install.json";

/// The file name of the installed copy's settings, next to the record
/// (ST-M007-011).
pub const SETTINGS_FILE: &str = "settings.toml";

/// The file name of the certificate authority file of the relay in the
/// settings, next to them (ST-M007-011).
pub const RELAY_CA_FILE: &str = "relay-ca.pem";

/// The uninstall entry's key, under `HKEY_CURRENT_USER`.
pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\CretLink";

/// The publisher shown in the Windows uninstall entry.
pub const PUBLISHER: &str = "Yves-Laurent Creton";

/// The macOS bundle identifier, the application's Tauri identifier.
pub const BUNDLE_ID: &str = "dev.cretlink.app";

/// The arguments the shortcut, the desktop entry and the start after install
/// pass, so the installed copy opens in the technician role.
pub const TECHNICIAN_ARGS: [&str; 2] = ["--role", "technician"];

/// The largest record uninstall reads (bounded input).
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// The current record format.
const RECORD_SCHEMA: u32 = 1;

/// The file-name prefix of a copy meant for the customer (ADR-0018).
const CUSTOMER_PREFIX: &str = "cretlink-support";

/// An operating system's install conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// `%LOCALAPPDATA%\Programs\CretLink\`.
    Windows,
    /// `~/Applications/CretLink.app`.
    MacOs,
    /// `~/.local/bin/` and a desktop entry.
    Linux,
}

impl Platform {
    /// The platform this build runs on. Unix systems other than macOS follow
    /// the Linux (XDG) conventions.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }

    fn exe_suffix(self) -> &'static str {
        match self {
            Self::Windows => ".exe",
            Self::MacOs | Self::Linux => "",
        }
    }

    /// The kind of menu entry install offers, if any.
    #[must_use]
    pub fn entry_kind(self) -> Option<EntryKind> {
        match self {
            Self::Windows => Some(EntryKind::StartMenu),
            Self::Linux => Some(EntryKind::DesktopEntry),
            // The bundle in ~/Applications is the entry.
            Self::MacOs => None,
        }
    }
}

/// The menu entry install can add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A Start-menu shortcut (Windows).
    StartMenu,
    /// A desktop entry in the applications menu (Linux).
    DesktopEntry,
}

impl EntryKind {
    /// How the entry is named to the user.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::StartMenu => "Start-menu shortcut",
            Self::DesktopEntry => "desktop entry",
        }
    }
}

/// The per-user base folders a [`Layout`] derives from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseDirs {
    /// The home directory.
    pub home: PathBuf,
    /// `%LOCALAPPDATA%` (Windows).
    pub local_app_data: Option<PathBuf>,
    /// `%APPDATA%` (Windows), which holds the Start menu.
    pub app_data: Option<PathBuf>,
    /// `$XDG_DATA_HOME` (Linux); `~/.local/share` when unset.
    pub data_home: Option<PathBuf>,
}

impl BaseDirs {
    /// Reads the folders from this process's environment.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME").or_else(|| var("USERPROFILE"))?;
        Some(Self {
            home,
            local_app_data: var("LOCALAPPDATA"),
            app_data: var("APPDATA"),
            data_home: var("XDG_DATA_HOME").filter(|path| path.is_absolute()),
        })
    }
}

/// Where an install puts each file, fixed per operating system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The operating system's conventions.
    pub platform: Platform,
    /// The install location shown to the user: the folder on Windows, the
    /// bundle on macOS, `~/.local/bin` on Linux.
    pub location: PathBuf,
    /// The installed application, which the AI clients are registered with.
    pub exe: PathBuf,
    /// The installed console build `cretlink-cli`.
    pub cli: PathBuf,
    /// The install record.
    pub record: PathBuf,
    /// The settings of the installed copy, next to the record (ST-M007-011).
    pub settings: PathBuf,
    /// The certificate authority file of the relay in the settings, next to
    /// them (ST-M007-011).
    pub relay_ca: PathBuf,
    /// The Start-menu shortcut or desktop entry, where the platform has one.
    pub entry: Option<PathBuf>,
    /// The bundle's `Info.plist` (macOS).
    pub info_plist: Option<PathBuf>,
}

impl Layout {
    /// The layout for `platform` under `dirs`. `None` on Windows without
    /// `%LOCALAPPDATA%` or `%APPDATA%`.
    #[must_use]
    pub fn new(platform: Platform, dirs: &BaseDirs) -> Option<Self> {
        let suffix = platform.exe_suffix();
        let exe_name = format!("cretlink{suffix}");
        let cli_name = format!("cretlink-cli{suffix}");
        Some(match platform {
            Platform::Windows => {
                let location = dirs
                    .local_app_data
                    .as_ref()?
                    .join("Programs")
                    .join("CretLink");
                let menu = dirs
                    .app_data
                    .as_ref()?
                    .join("Microsoft")
                    .join("Windows")
                    .join("Start Menu")
                    .join("Programs");
                Self {
                    platform,
                    exe: location.join(&exe_name),
                    cli: location.join(&cli_name),
                    record: location.join(RECORD_FILE),
                    settings: location.join(SETTINGS_FILE),
                    relay_ca: location.join(RELAY_CA_FILE),
                    entry: Some(menu.join("CretLink.lnk")),
                    info_plist: None,
                    location,
                }
            }
            Platform::MacOs => {
                let location = dirs.home.join("Applications").join("CretLink.app");
                let contents = location.join("Contents");
                let resources = contents.join("Resources");
                Self {
                    platform,
                    exe: contents.join("MacOS").join(&exe_name),
                    cli: contents.join("MacOS").join(&cli_name),
                    record: resources.join(RECORD_FILE),
                    settings: resources.join(SETTINGS_FILE),
                    relay_ca: resources.join(RELAY_CA_FILE),
                    entry: None,
                    info_plist: Some(contents.join("Info.plist")),
                    location,
                }
            }
            Platform::Linux => {
                let location = dirs.home.join(".local").join("bin");
                let data = dirs
                    .data_home
                    .clone()
                    .unwrap_or_else(|| dirs.home.join(".local").join("share"));
                let own = data.join("CretLink");
                Self {
                    platform,
                    exe: location.join(&exe_name),
                    cli: location.join(&cli_name),
                    record: own.join(RECORD_FILE),
                    settings: own.join(SETTINGS_FILE),
                    relay_ca: own.join(RELAY_CA_FILE),
                    entry: Some(data.join("applications").join("cretlink.desktop")),
                    info_plist: None,
                    location,
                }
            }
        })
    }

    /// The layout of this computer and user.
    #[must_use]
    pub fn current() -> Option<Self> {
        Self::new(Platform::current(), &BaseDirs::from_env()?)
    }

    /// Whether `exe` is the installed program, so this copy runs from the
    /// install location.
    #[must_use]
    pub fn is_installed_exe(&self, exe: &Path) -> bool {
        same_path(exe, &self.exe)
    }

    /// Whether `exe` is one of the installed programs — `cretlink` or
    /// `cretlink-cli` — whose file cannot be removed while it runs.
    #[must_use]
    pub fn runs_from_install(&self, exe: &Path) -> bool {
        same_path(exe, &self.exe) || same_path(exe, &self.cli)
    }

    /// Whether a copy is installed here.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.exe.is_file()
    }

    /// The folders install creates and uninstall removes once they are empty,
    /// innermost first.
    fn owned_dirs(&self) -> Vec<PathBuf> {
        match self.platform {
            Platform::Windows => vec![self.location.clone()],
            Platform::MacOs => {
                let contents = self.location.join("Contents");
                vec![
                    contents.join("MacOS"),
                    contents.join("Resources"),
                    contents,
                    self.location.clone(),
                ]
            }
            // `~/.local/bin` is shared; the record's folder also holds the
            // audit trail, which stays.
            Platform::Linux => self
                .record
                .parent()
                .map(Path::to_path_buf)
                .into_iter()
                .collect(),
        }
    }

    /// The files of an install: the programs, the settings of the installed
    /// copy, and the record last.
    fn files(&self) -> Vec<PathBuf> {
        let mut files = vec![self.exe.clone(), self.cli.clone()];
        files.extend(self.info_plist.clone());
        files.extend(self.settings_files());
        files.push(self.record.clone());
        files
    }

    /// The settings of the installed copy and the certificate authority file
    /// of their relay (ST-M007-011), each with the temporary file it is
    /// written through: a key must not stay behind in one of those either.
    fn settings_files(&self) -> Vec<PathBuf> {
        vec![
            self.settings.clone(),
            temporary_name(&self.settings),
            self.relay_ca.clone(),
            temporary_name(&self.relay_ca),
        ]
    }
}

/// The per-user audit folder, which install and uninstall never touch. Mirrors
/// `cretlink_console::default_audit_dir` (the application tests that they
/// agree).
#[must_use]
pub fn audit_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("CretLink").join("audit")
}

/// Whether a program file marks a customer copy (`CretLink-Support*`, any
/// case), which never installs or uninstalls (ADR-0018).
#[must_use]
pub fn is_customer_copy(exe: &Path) -> bool {
    exe.file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|stem| stem.starts_with(CUSTOMER_PREFIX))
}

/// Whether two paths name the same file: compared after resolving links when
/// both exist, and without case on Windows.
fn same_path(a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (a, b) = (resolve(a), resolve(b));
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

// --- The record ---

/// A client registration install made (or found pointing at the installed
/// program and adopted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedRegistration {
    /// The client's `--client` name: `claude`, `codex` or `gemini`.
    pub client: String,
    /// The program it starts: the installed `cretlink`.
    pub command: String,
    /// Its arguments: `["mcp"]`.
    pub args: Vec<String>,
}

/// `install.json`: what install created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRecord {
    /// The record format, currently 1.
    pub schema: u32,
    /// The CretLink release that was installed.
    pub version: String,
    /// The install location.
    pub location: PathBuf,
    /// The program files install wrote.
    pub files: Vec<PathBuf>,
    /// The shortcut or desktop entry, if install added one.
    pub entry: Option<PathBuf>,
    /// The uninstall entry, if install added one.
    pub uninstall_entry: Option<String>,
    /// The client registrations install made.
    pub registrations: Vec<RecordedRegistration>,
}

impl InstallRecord {
    fn has_client(&self, client: Client) -> bool {
        self.registrations
            .iter()
            .any(|recorded| recorded.client == client.program())
    }

    /// Records a registration of `client` with `exe mcp`, once.
    pub fn record_client(&mut self, client: Client, exe: &Path) {
        if self.has_client(client) {
            return;
        }
        self.registrations.push(RecordedRegistration {
            client: client.program().to_owned(),
            command: exe.to_string_lossy().into_owned(),
            args: vec!["mcp".to_owned()],
        });
    }

    /// The recorded clients this build knows, in record order.
    #[must_use]
    pub fn clients(&self) -> Vec<Client> {
        self.registrations
            .iter()
            .filter_map(|recorded| Client::parse(&recorded.client).ok())
            .collect()
    }
}

/// Reads the record. `Ok(None)` when there is none.
///
/// # Errors
///
/// Returns a message when the record exists but cannot be read, is larger
/// than 64 KiB or is malformed.
pub fn read_record(layout: &Layout) -> Result<Option<InstallRecord>, String> {
    let path = &layout.record;
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    if meta.len() > MAX_RECORD_BYTES {
        return Err(format!(
            "{} is too large to be an install record",
            path.display()
        ));
    }
    let text =
        fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| format!("{} is not a valid install record: {err}", path.display()))
}

fn write_record(layout: &Layout, record: &InstallRecord) -> Result<(), InstallError> {
    let text = serde_json::to_string_pretty(record).map_err(|err| {
        InstallError::io(
            "write the install record",
            &layout.record,
            io::Error::other(err),
        )
    })?;
    write_file(&layout.record, text.as_bytes(), false)
        .map_err(|err| InstallError::io("write the install record", &layout.record, err))
}

/// Adds a registration of `client` to the record of an installed copy, so
/// uninstall removes it too. Does nothing when nothing is installed.
///
/// # Errors
///
/// Returns a message when the record cannot be read or written.
pub fn record_registration(layout: &Layout, client: Client) -> Result<(), String> {
    let Some(mut record) = read_record(layout)? else {
        return Ok(());
    };
    if record.has_client(client) {
        return Ok(());
    }
    record.record_client(client, &layout.exe);
    write_record(layout, &record).map_err(|err| err.to_string())
}

// --- The operating system ---

/// The values of the Windows uninstall entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallEntry {
    /// `DisplayName`.
    pub display_name: String,
    /// `DisplayVersion`.
    pub display_version: String,
    /// `Publisher`.
    pub publisher: String,
    /// `InstallLocation`.
    pub install_location: String,
    /// `UninstallString`: `"<path>" uninstall`.
    pub uninstall_string: String,
    /// `DisplayIcon`: the installed program.
    pub display_icon: String,
}

/// What a Windows shortcut starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    /// The `.lnk` file.
    pub path: PathBuf,
    /// The program it starts.
    pub target: PathBuf,
    /// Its arguments, as one command line.
    pub arguments: String,
    /// Its description (the tooltip).
    pub description: String,
}

/// The parts of install and uninstall that are not plain files.
pub trait System {
    /// Whether a process other than this one runs `exe`.
    ///
    /// # Errors
    ///
    /// Returns an error when the process list cannot be read.
    fn is_running(&self, exe: &Path) -> io::Result<bool>;

    /// Creates or replaces a Start-menu shortcut (Windows).
    ///
    /// # Errors
    ///
    /// Returns an error when the shortcut cannot be written.
    fn create_shortcut(&self, shortcut: &Shortcut) -> io::Result<()>;

    /// Writes the uninstall entry under [`UNINSTALL_KEY`] (Windows).
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be written.
    fn write_uninstall_entry(&self, entry: &UninstallEntry) -> io::Result<()>;

    /// Whether the uninstall entry exists (Windows).
    fn has_uninstall_entry(&self) -> bool;

    /// Removes the uninstall entry; `Ok(false)` when there was none.
    ///
    /// # Errors
    ///
    /// Returns an error when it exists and cannot be removed.
    fn remove_uninstall_entry(&self) -> io::Result<bool>;

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

impl System for OsSystem {
    fn is_running(&self, exe: &Path) -> io::Result<bool> {
        running_elsewhere(exe)
    }

    fn create_shortcut(&self, shortcut: &Shortcut) -> io::Result<()> {
        #[cfg(windows)]
        {
            win::create_shortcut(shortcut)
        }
        #[cfg(not(windows))]
        {
            let _ = shortcut;
            Err(windows_only())
        }
    }

    fn write_uninstall_entry(&self, entry: &UninstallEntry) -> io::Result<()> {
        #[cfg(windows)]
        {
            win::write_uninstall_entry(entry)
        }
        #[cfg(not(windows))]
        {
            let _ = entry;
            Err(windows_only())
        }
    }

    fn has_uninstall_entry(&self) -> bool {
        #[cfg(windows)]
        {
            win::has_uninstall_entry()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn remove_uninstall_entry(&self) -> io::Result<bool> {
        #[cfg(windows)]
        {
            win::remove_uninstall_entry()
        }
        #[cfg(not(windows))]
        {
            Ok(false)
        }
    }

    fn remove_after_exit(&self, files: &[PathBuf], dir: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            win::remove_after_exit(files, dir)
        }
        #[cfg(not(windows))]
        {
            let _ = (files, dir);
            Err(windows_only())
        }
    }
}

#[cfg(not(windows))]
fn windows_only() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "this exists only on Windows")
}

/// Whether a process other than this one runs `exe`, from `/proc` on Linux.
#[cfg(all(unix, not(target_os = "macos")))]
fn running_elsewhere(exe: &Path) -> io::Result<bool> {
    let me = std::process::id();
    let target = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    for entry in fs::read_dir("/proc")?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        // Another user's process cannot be read, and cannot be ours either.
        let Ok(link) = fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        // A program replaced while it runs shows as "<path> (deleted)".
        let link = link.to_string_lossy();
        let link = link.strip_suffix(" (deleted)").unwrap_or(&link);
        if Path::new(link) == target {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a process other than this one runs `exe`, from `ps` on macOS, whose
/// `comm` column is the executable's path.
#[cfg(target_os = "macos")]
fn running_elsewhere(exe: &Path) -> io::Result<bool> {
    let me = std::process::id();
    let target = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let output = std::process::Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=,comm="])
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("ps could not list the running programs"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().any(|line| {
        let line = line.trim_start();
        let Some((pid, comm)) = line.split_once(char::is_whitespace) else {
            return false;
        };
        pid.parse::<u32>().is_ok_and(|pid| pid != me) && same_path(Path::new(comm.trim()), &target)
    }))
}

#[cfg(windows)]
fn running_elsewhere(exe: &Path) -> io::Result<bool> {
    win::running_elsewhere(exe)
}

/// The PowerShell script that removes an installed Windows copy once the
/// process `pid` has exited (see `win::remove_after_exit`): only the given
/// files, then the folder if nothing else is left in it. It waits without a
/// time limit — the helper is detached and has no window — because removing
/// while the program still runs would fail on the in-use file
/// (ST-M009-006). Paths are
/// single-quoted literals; every quote character PowerShell accepts as one is
/// doubled.
#[cfg_attr(not(windows), allow(dead_code, reason = "tested on every platform"))]
fn removal_script(pid: u32, files: &[PathBuf], dir: &Path) -> String {
    let quote = |path: &Path| {
        let mut quoted = String::from("'");
        for c in path.to_string_lossy().chars() {
            if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
                quoted.push(c);
            }
            quoted.push(c);
        }
        quoted.push('\'');
        quoted
    };
    let files: Vec<String> = files.iter().map(|file| quote(file)).collect();
    let dir = quote(dir);
    format!(
        "Wait-Process -Id {pid} -ErrorAction SilentlyContinue; \
         Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue; \
         if (-not (Get-ChildItem -LiteralPath {dir} -Force)) {{ Remove-Item -LiteralPath {dir} -Force }}",
        files.join(",")
    )
}

// --- Install ---

/// What to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    /// The running program to copy: `cretlink`. A `cretlink-cli` next to it is
    /// copied too.
    pub source: PathBuf,
    /// Whether to add the Start-menu shortcut or desktop entry.
    pub entry: bool,
    /// The clients to register with the installed program.
    pub clients: Vec<Client>,
}

/// What install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// The install location.
    pub location: PathBuf,
    /// The installed program.
    pub exe: PathBuf,
    /// Whether an older copy was replaced (an update in place).
    pub update: bool,
    /// The clients now registered with the installed program.
    pub registered: Vec<Client>,
    /// Registrations that failed, in words. The install itself completed.
    pub failures: Vec<String>,
}

/// Why install or uninstall stopped.
#[derive(Debug)]
pub enum InstallError {
    /// A `CretLink-Support*` copy is for getting help only.
    CustomerCopy,
    /// This copy already is the installed one.
    AlreadyInstalled(PathBuf),
    /// Nothing is installed.
    NotInstalled(PathBuf),
    /// The installed copy runs in another process.
    Running(PathBuf),
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

impl InstallError {
    fn io(stage: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            stage,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CustomerCopy => f.write_str(
                "this copy of CretLink is for getting help; it does not install or uninstall",
            ),
            Self::AlreadyInstalled(path) => {
                write!(
                    f,
                    "this copy already is the installed one, in {}",
                    path.display()
                )
            }
            Self::NotInstalled(path) => {
                write!(
                    f,
                    "CretLink is not installed for this user (nothing of it in {})",
                    path.display()
                )
            }
            Self::Running(path) => write!(
                f,
                "the installed CretLink is running ({}). Close it, and any AI assistant that uses \
                 it, then try again",
                path.display()
            ),
            Self::Io {
                stage,
                path,
                source,
            } => write!(f, "could not {stage} ({}): {source}", path.display()),
        }
    }
}

impl std::error::Error for InstallError {}

fn ensure_not_running(layout: &Layout, system: &dyn System) -> Result<(), InstallError> {
    for exe in [&layout.exe, &layout.cli] {
        let running = system
            .is_running(exe)
            .map_err(|err| InstallError::io("check whether CretLink is running", exe, err))?;
        if running {
            return Err(InstallError::Running(exe.clone()));
        }
    }
    Ok(())
}

/// Installs the running program for this user: copies it (and `cretlink-cli`
/// next to it) to the layout, replacing an older copy; adds the menu entry
/// when asked and, on Windows, the uninstall entry; writes the record; and
/// registers the requested clients with the installed program through the
/// setup interface. `context` locates the clients; its `exe` is replaced by the
/// installed program.
///
/// A registration that fails does not undo the install: it is reported in
/// [`InstallReport::failures`].
///
/// # Errors
///
/// Stops before changing anything for a customer copy, the installed copy
/// itself, or a running installed copy; stops on the first file or system
/// step that fails.
pub fn install(
    layout: &Layout,
    system: &dyn System,
    runner: &dyn Runner,
    context: &setup::Context,
    request: &InstallRequest,
) -> Result<InstallReport, InstallError> {
    if is_customer_copy(&request.source) {
        return Err(InstallError::CustomerCopy);
    }
    if layout.is_installed_exe(&request.source) {
        return Err(InstallError::AlreadyInstalled(layout.location.clone()));
    }
    ensure_not_running(layout, system)?;

    // A record that cannot be read is replaced; its registrations are lost
    // to uninstall, which then leaves them alone.
    let previous = read_record(layout).ok().flatten();
    let update = layout.exe.exists();
    let version = crate::RELEASE_VERSION.to_owned();

    copy_program(&request.source, &layout.exe)
        .map_err(|err| InstallError::io("copy the program", &layout.exe, err))?;
    let mut files = vec![layout.exe.clone()];
    let cli_source = request
        .source
        .with_file_name(format!("cretlink-cli{}", layout.platform.exe_suffix()));
    if cli_source.is_file() {
        copy_program(&cli_source, &layout.cli)
            .map_err(|err| InstallError::io("copy cretlink-cli", &layout.cli, err))?;
        files.push(layout.cli.clone());
    } else {
        // An older cretlink-cli would no longer match the program.
        remove_file_if_exists(&layout.cli)
            .map_err(|err| InstallError::io("remove the older cretlink-cli", &layout.cli, err))?;
    }
    if let Some(plist) = &layout.info_plist {
        write_file(plist, info_plist(&version).as_bytes(), false)
            .map_err(|err| InstallError::io("write the application bundle", plist, err))?;
        files.push(plist.clone());
    }

    let mut entry = None;
    if let (Some(kind), Some(path)) = (layout.platform.entry_kind(), &layout.entry) {
        if request.entry {
            match kind {
                EntryKind::StartMenu => system
                    .create_shortcut(&Shortcut {
                        path: path.clone(),
                        target: layout.exe.clone(),
                        arguments: TECHNICIAN_ARGS.join(" "),
                        description: "CretLink: give help".to_owned(),
                    })
                    .map_err(|err| InstallError::io("add the Start-menu shortcut", path, err))?,
                EntryKind::DesktopEntry => {
                    let text = desktop_entry(&layout.exe)
                        .map_err(|err| InstallError::io("write the desktop entry", path, err))?;
                    write_file(path, text.as_bytes(), false)
                        .map_err(|err| InstallError::io("write the desktop entry", path, err))?;
                }
            }
            entry = Some(path.clone());
        } else if previous
            .as_ref()
            .is_some_and(|record| record.entry.is_some())
        {
            // Not wanted any more: remove the one an earlier install added.
            remove_file_if_exists(path)
                .map_err(|err| InstallError::io("remove the menu entry", path, err))?;
        }
    }

    let mut uninstall_entry = None;
    if layout.platform == Platform::Windows {
        let exe = layout.exe.to_string_lossy().into_owned();
        system
            .write_uninstall_entry(&UninstallEntry {
                display_name: "CretLink".to_owned(),
                display_version: version.clone(),
                publisher: PUBLISHER.to_owned(),
                install_location: layout.location.to_string_lossy().into_owned(),
                uninstall_string: format!("\"{exe}\" uninstall"),
                display_icon: exe,
            })
            .map_err(|err| {
                InstallError::io("add the uninstall entry", Path::new(UNINSTALL_KEY), err)
            })?;
        uninstall_entry = Some(format!(r"HKCU\{UNINSTALL_KEY}"));
    }

    let mut record = InstallRecord {
        schema: RECORD_SCHEMA,
        version,
        location: layout.location.clone(),
        files,
        entry,
        uninstall_entry,
        registrations: previous
            .map(|record| record.registrations)
            .unwrap_or_default(),
    };
    // Written before any registration, so an interrupted install can still be
    // uninstalled.
    write_record(layout, &record)?;

    let mut context = context.clone();
    context.exe = layout.exe.clone();
    let mut registered = Vec::new();
    let mut failures = Vec::new();
    for &client in &request.clients {
        let status = setup::inspect(&context, client);
        match setup::apply(runner, &status, Action::Install) {
            Ok(()) => {
                record.record_client(client, &layout.exe);
                registered.push(client);
            }
            Err(err) => failures.push(format!("{}: {err}", client.label())),
        }
    }
    write_record(layout, &record)?;

    Ok(InstallReport {
        location: layout.location.clone(),
        exe: layout.exe.clone(),
        update,
        registered,
        failures,
    })
}

/// Starts the installed copy in the technician role, detached from this
/// process.
///
/// # Errors
///
/// Returns an error when the program cannot be started.
pub fn start_installed(layout: &Layout) -> io::Result<()> {
    std::process::Command::new(&layout.exe)
        .args(TECHNICIAN_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

/// Copies a program over `to` through a temporary file in the same folder, so
/// an older copy is replaced in one step.
fn copy_program(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_name(to);
    let _ = fs::remove_file(&temporary);
    let result = fs::copy(from, &temporary).and_then(|_| {
        set_executable(&temporary, true)?;
        fs::rename(&temporary, to)
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Writes a file through a temporary one in the same folder.
fn write_file(path: &Path, bytes: &[u8], executable: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_name(path);
    let result = fs::write(&temporary, bytes).and_then(|()| {
        set_executable(&temporary, executable)?;
        fs::rename(&temporary, path)
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn temporary_name(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".cretlink-new");
    path.with_file_name(name)
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

pub(crate) fn remove_file_if_exists(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// The minimal `Info.plist` that makes the macOS folder an application
/// bundle around `cretlink`.
fn info_plist(version: &str) -> String {
    let version = xml_escape(version);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>CretLink</string>
	<key>CFBundleExecutable</key>
	<string>cretlink</string>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>CretLink</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
"#
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The Linux desktop entry that opens the installed copy in the technician
/// role (Desktop Entry Specification 1.5).
fn desktop_entry(exe: &Path) -> io::Result<String> {
    let exe = exe.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "the path is not valid UTF-8")
    })?;
    if exe.contains(['\n', '\r']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path contains a line break",
        ));
    }
    Ok(format!(
        "[Desktop Entry]\nType=Application\nVersion=1.5\nName=CretLink\nGenericName=Remote support\n\
         Comment=Give help to another computer, yourself or through your AI assistant\n\
         Exec={} {}\nTerminal=false\nCategories=Network;RemoteAccess;\n",
        desktop_exec_arg(exe),
        TECHNICIAN_ARGS.join(" ")
    ))
}

/// Quotes one `Exec` argument: inside double quotes, `"`, `` ` ``, `$` and
/// `\` are escaped with a backslash, then every backslash is escaped again as
/// a string value; `%` becomes `%%` (field codes).
fn desktop_exec_arg(arg: &str) -> String {
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(c);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

// --- Uninstall ---

/// A client registration uninstall removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRemoval {
    /// The client.
    pub client: Client,
    /// The command uninstall runs, as displayed.
    pub command: String,
}

/// What uninstall will remove, shown before anything changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    /// The install location.
    pub location: PathBuf,
    /// The program files that exist, the record last.
    pub files: Vec<PathBuf>,
    /// The settings of the installed copy that exist: its relay with the
    /// admission key, the relay's certificate file and the network it waits
    /// on (ST-M007-011).
    pub settings: Vec<PathBuf>,
    /// Whether the folder is removed only after this process exits (Windows,
    /// when uninstall runs from the installed copy).
    pub after_exit: bool,
    /// The shortcut or desktop entry, when it exists.
    pub entry: Option<(EntryKind, PathBuf)>,
    /// Whether the Windows uninstall entry exists.
    pub uninstall_entry: bool,
    /// The recorded registrations that still point at the installed program.
    pub registrations: Vec<PlannedRemoval>,
    /// What stays, in words: recorded registrations that point elsewhere now,
    /// a record that could not be read, and a relay profile next to the
    /// installed program, with whether it holds an admission key
    /// (ST-M007-011).
    pub kept: Vec<String>,
    /// The audit trail, which stays.
    pub audit_dir: PathBuf,
}

impl UninstallPlan {
    /// What will be removed, one line each, for display.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let after = if self.after_exit {
            " (removed once CretLink has closed)"
        } else {
            ""
        };
        if !self.files.is_empty() {
            if self.location.extension().is_some_and(|ext| ext == "app") {
                lines.push(format!(
                    "The application {}{after}",
                    self.location.display()
                ));
            } else {
                for file in &self.files {
                    lines.push(format!("{}{after}", file.display()));
                }
            }
        }
        // Named for themselves on every system: they hold the relay's
        // admission key, and the technician should see that it goes too.
        for file in &self.settings {
            lines.push(format!(
                "{} {}{after}",
                settings_label(file),
                file.display()
            ));
        }
        if let Some((kind, path)) = &self.entry {
            let label = match kind {
                EntryKind::StartMenu => "The Start-menu shortcut",
                EntryKind::DesktopEntry => "The desktop entry",
            };
            lines.push(format!("{label} {}", path.display()));
        }
        if self.uninstall_entry {
            lines.push(format!(r"The uninstall entry HKCU\{UNINSTALL_KEY}"));
        }
        for removal in &self.registrations {
            lines.push(format!(
                "The {} registration: {}",
                removal.client.label(),
                removal.command
            ));
        }
        lines
    }

    /// Whether there is nothing to remove.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.settings.is_empty()
            && self.entry.is_none()
            && !self.uninstall_entry
            && self.registrations.is_empty()
    }
}

/// What a file of the installed copy's settings is, for the uninstall plan.
fn settings_label(file: &Path) -> &'static str {
    match file.file_name().and_then(|name| name.to_str()) {
        Some(SETTINGS_FILE) => "Your settings (the relay with its key, the network to wait on):",
        Some(RELAY_CA_FILE) => "The certificate file of your relay:",
        _ => "An unfinished copy of your settings:",
    }
}

/// What the uninstall plan says about a relay profile next to the installed
/// program: that it stays, where it lies, and what it holds. The file is read
/// to say whether it holds a key; nothing of what it holds is repeated.
fn kept_profile_words(profile: &Path) -> String {
    let holds = match relay_profile::read_file(profile) {
        Ok(Some(relay)) if relay.key().is_some() => {
            "It holds the admission key of a relay in readable form: remove the file yourself \
             when nobody else should be able to read that key"
        }
        Ok(Some(_)) => "It holds the address of a relay and no admission key",
        Ok(None) => "It holds no relay",
        Err(_) => {
            "It could not be read as a relay profile and may hold the admission key of a relay: \
             look at it, and remove it yourself when nobody else should be able to read such a \
             key"
        }
    };
    format!(
        "The relay profile {}, which CretLink did not write and does not remove. {holds}",
        profile.display()
    )
}

/// Plans an uninstall from the record. `current_exe` is the running program:
/// when it is the installed one on Windows, the folder goes after it exits.
///
/// # Errors
///
/// Refuses a customer copy, and reports when nothing is installed.
pub fn uninstall_plan(
    layout: &Layout,
    system: &dyn System,
    context: &setup::Context,
    current_exe: &Path,
) -> Result<UninstallPlan, InstallError> {
    if is_customer_copy(current_exe) {
        return Err(InstallError::CustomerCopy);
    }
    let mut kept = Vec::new();
    let record = match read_record(layout) {
        Ok(record) => record,
        Err(reason) => {
            kept.push(format!(
                "The AI assistant registrations, because the install record could not be read: \
                 {reason}"
            ));
            None
        }
    };
    let settings: Vec<PathBuf> = layout
        .settings_files()
        .into_iter()
        .filter(|file| file.exists())
        .collect();
    let files: Vec<PathBuf> = layout
        .files()
        .into_iter()
        .filter(|file| file.exists() && !settings.contains(file))
        .collect();
    let entry = layout
        .platform
        .entry_kind()
        .zip(layout.entry.clone())
        .filter(|(_, path)| path.exists());
    let uninstall_entry = layout.platform == Platform::Windows && system.has_uninstall_entry();
    if files.is_empty()
        && settings.is_empty()
        && entry.is_none()
        && !uninstall_entry
        && record.is_none()
    {
        return Err(InstallError::NotInstalled(layout.location.clone()));
    }

    // A relay profile next to the installed program is not part of the
    // install: a person put it there, and it wins over Settings while it is
    // there (ST-M007-011). Uninstall leaves it, and says so, because it may
    // hold the admission key of a relay in readable form.
    if let Some(profile) =
        relay_profile::path_next_to(&layout.exe).filter(|profile| profile.exists())
    {
        kept.push(kept_profile_words(&profile));
    }

    let mut context = context.clone();
    context.exe = layout.exe.clone();
    let mut registrations = Vec::new();
    for client in record
        .as_ref()
        .map(InstallRecord::clients)
        .unwrap_or_default()
    {
        let status = setup::inspect(&context, client);
        match &status.registration {
            RegistrationState::ThisExecutable(_) if status.present() => {
                registrations.push(PlannedRemoval {
                    client,
                    command: status
                        .uninstall_plan
                        .iter()
                        .map(setup::PlannedCommand::display)
                        .collect::<Vec<_>>()
                        .join(" && "),
                });
            }
            RegistrationState::ThisExecutable(_) => kept.push(format!(
                "The {} registration, because '{}' is not on PATH: remove the 'cretlink' entry \
                 from {} by hand",
                client.label(),
                client.program(),
                status.config_path.as_ref().map_or_else(
                    || "its configuration".to_owned(),
                    |p| p.display().to_string()
                )
            )),
            RegistrationState::OtherCommand(registration) => kept.push(format!(
                "The {} registration, which now starts {}",
                client.label(),
                registration.display()
            )),
            RegistrationState::Unreadable(reason) => kept.push(format!(
                "The {} registration, which could not be read: {reason}",
                client.label()
            )),
            RegistrationState::Absent => {}
        }
    }

    Ok(UninstallPlan {
        location: layout.location.clone(),
        after_exit: layout.platform == Platform::Windows && layout.runs_from_install(current_exe),
        files,
        settings,
        entry,
        uninstall_entry,
        registrations,
        kept,
        audit_dir: audit_dir(),
    })
}

/// What uninstall did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UninstallReport {
    /// What was removed, in words.
    pub removed: Vec<String>,
    /// What is removed once this process has exited.
    pub pending: Vec<String>,
    /// What stays on purpose, in words.
    pub kept: Vec<String>,
    /// What could not be removed, in words. Empty on a complete uninstall.
    pub failed: Vec<String>,
}

impl UninstallReport {
    /// Whether everything planned was removed or scheduled.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Carries out `plan`. Each registration is checked again right before it is
/// removed and left alone if it no longer points at the installed program.
/// Every step is attempted; failures are reported, never hidden.
///
/// # Errors
///
/// Stops before changing anything for a customer copy or when the installed
/// copy runs in another process.
pub fn uninstall(
    layout: &Layout,
    system: &dyn System,
    runner: &dyn Runner,
    context: &setup::Context,
    plan: &UninstallPlan,
    current_exe: &Path,
) -> Result<UninstallReport, InstallError> {
    if is_customer_copy(current_exe) {
        return Err(InstallError::CustomerCopy);
    }
    ensure_not_running(layout, system)?;
    let mut report = UninstallReport {
        kept: plan.kept.clone(),
        ..UninstallReport::default()
    };

    let mut context = context.clone();
    context.exe = layout.exe.clone();
    for removal in &plan.registrations {
        let client = removal.client;
        let status = setup::inspect(&context, client);
        if !matches!(status.registration, RegistrationState::ThisExecutable(_)) {
            report.kept.push(format!(
                "The {} registration, which no longer points at the installed CretLink",
                client.label()
            ));
            continue;
        }
        match setup::apply(runner, &status, Action::Uninstall) {
            Ok(()) => report
                .removed
                .push(format!("The {} registration", client.label())),
            Err(err) => report
                .failed
                .push(format!("The {} registration: {err}", client.label())),
        }
    }

    if let Some((kind, path)) = &plan.entry {
        match remove_file_if_exists(path) {
            Ok(_) => report
                .removed
                .push(format!("The {} {}", kind.label(), path.display())),
            Err(err) => {
                report
                    .failed
                    .push(format!("The {} {}: {err}", kind.label(), path.display()))
            }
        }
    }

    if plan.uninstall_entry {
        match system.remove_uninstall_entry() {
            Ok(_) => report.removed.push("The uninstall entry".to_owned()),
            Err(err) => report
                .failed
                .push(format!(r"The uninstall entry HKCU\{UNINSTALL_KEY}: {err}")),
        }
    }

    // Only the layout's own paths are removed, never one read from the record.
    let files = layout.files();
    if plan.after_exit {
        match system.remove_after_exit(&files, &layout.location) {
            Ok(()) => report.pending.push(format!(
                "The folder {}, once CretLink has closed",
                layout.location.display()
            )),
            Err(err) => report
                .failed
                .push(format!("The folder {}: {err}", layout.location.display())),
        }
    } else {
        for file in &files {
            match remove_file_if_exists(file) {
                Ok(true) => report.removed.push(file.display().to_string()),
                Ok(false) => {}
                Err(err) => report.failed.push(format!("{}: {err}", file.display())),
            }
        }
        for dir in layout.owned_dirs() {
            // Removed only when empty; the Linux data folder keeps the audit
            // trail.
            let _ = fs::remove_dir(&dir);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use crate::setup::RunOutput;

    /// Records the system calls and answers from fixed state.
    #[derive(Default)]
    struct FakeSystem {
        running: RefCell<Vec<PathBuf>>,
        shortcuts: RefCell<Vec<Shortcut>>,
        entry: RefCell<Option<UninstallEntry>>,
        after_exit: RefCell<Vec<(Vec<PathBuf>, PathBuf)>>,
    }

    impl System for FakeSystem {
        fn is_running(&self, exe: &Path) -> io::Result<bool> {
            Ok(self.running.borrow().iter().any(|path| path == exe))
        }

        fn create_shortcut(&self, shortcut: &Shortcut) -> io::Result<()> {
            // Stand in for the .lnk file, so uninstall can remove it.
            write_file(&shortcut.path, b"lnk", false)?;
            self.shortcuts.borrow_mut().push(shortcut.clone());
            Ok(())
        }

        fn write_uninstall_entry(&self, entry: &UninstallEntry) -> io::Result<()> {
            *self.entry.borrow_mut() = Some(entry.clone());
            Ok(())
        }

        fn has_uninstall_entry(&self) -> bool {
            self.entry.borrow().is_some()
        }

        fn remove_uninstall_entry(&self) -> io::Result<bool> {
            Ok(self.entry.borrow_mut().take().is_some())
        }

        fn remove_after_exit(&self, files: &[PathBuf], dir: &Path) -> io::Result<()> {
            self.after_exit
                .borrow_mut()
                .push((files.to_vec(), dir.to_path_buf()));
            Ok(())
        }
    }

    /// Stands in for the client command lines: `add` and `remove` change the
    /// fixture's Claude Code and Gemini CLI JSON files the way the real
    /// clients do, so a later inspection sees the change.
    struct Clients {
        home: PathBuf,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Clients {
        fn config(&self, program: &Path) -> PathBuf {
            match program.file_stem().and_then(|stem| stem.to_str()) {
                Some("gemini") => self.home.join(".gemini").join("settings.json"),
                _ => self.home.join(".claude.json"),
            }
        }
    }

    impl Runner for Clients {
        fn run(&self, program: &Path, args: &[String]) -> io::Result<RunOutput> {
            self.calls.borrow_mut().push(args.to_vec());
            let config = self.config(program);
            let mut root: serde_json::Value = fs::read_to_string(&config)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_else(|| serde_json::json!({}));
            match args.get(1).map(String::as_str) {
                Some("add") => {
                    let start = args.iter().position(|arg| arg == "--").map_or(5, |i| i + 1);
                    let command = args[start].clone();
                    let rest: Vec<String> = args[start + 1..].to_vec();
                    root["mcpServers"]["cretlink"] =
                        serde_json::json!({ "command": command, "args": rest });
                }
                Some("remove") => {
                    if let Some(servers) = root
                        .get_mut("mcpServers")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        servers.remove("cretlink");
                    }
                }
                _ => {}
            }
            write_file(&config, root.to_string().as_bytes(), false)?;
            Ok(RunOutput {
                success: true,
                output: String::new(),
            })
        }
    }

    struct Fixture {
        root: PathBuf,
        downloads: PathBuf,
        layout: Layout,
        context: setup::Context,
        system: FakeSystem,
        clients: Clients,
    }

    impl Fixture {
        fn new(name: &str, platform: Platform) -> Self {
            let root = std::env::temp_dir()
                .join(format!("cretlink-install-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let home = root.join("home");
            let dirs = BaseDirs {
                home: home.clone(),
                local_app_data: Some(home.join("AppData").join("Local")),
                app_data: Some(home.join("AppData").join("Roaming")),
                data_home: None,
            };
            let layout = Layout::new(platform, &dirs).unwrap();
            let downloads = home.join("Downloads");
            let suffix = platform.exe_suffix();
            write_file(
                &downloads.join(format!("cretlink{suffix}")),
                b"cretlink v1",
                true,
            )
            .unwrap();
            write_file(
                &downloads.join(format!("cretlink-cli{suffix}")),
                b"cli v1",
                true,
            )
            .unwrap();
            // Fake client programs on PATH: Claude Code and Gemini CLI.
            let bin = root.join("bin");
            let ext = if cfg!(windows) { ".cmd" } else { "" };
            for program in ["claude", "gemini"] {
                write_file(&bin.join(format!("{program}{ext}")), b"", true).unwrap();
            }
            let context = setup::Context {
                exe: downloads.join(format!("cretlink{suffix}")),
                home: Some(home.clone()),
                path: vec![bin],
                path_ext: vec![ext.to_owned()],
                claude_config_dir: None,
                codex_home: None,
            };
            Self {
                clients: Clients {
                    home,
                    calls: RefCell::new(Vec::new()),
                },
                root,
                downloads,
                layout,
                context,
                system: FakeSystem::default(),
            }
        }

        fn source(&self) -> PathBuf {
            self.downloads
                .join(format!("cretlink{}", self.layout.platform.exe_suffix()))
        }

        fn install(&self, entry: bool, clients: &[Client]) -> Result<InstallReport, InstallError> {
            install(
                &self.layout,
                &self.system,
                &self.clients,
                &self.context,
                &InstallRequest {
                    source: self.source(),
                    entry,
                    clients: clients.to_vec(),
                },
            )
        }

        fn plan(&self, current: &Path) -> UninstallPlan {
            uninstall_plan(&self.layout, &self.system, &self.context, current).unwrap()
        }

        fn uninstall(&self, current: &Path) -> UninstallReport {
            let plan = self.plan(current);
            uninstall(
                &self.layout,
                &self.system,
                &self.clients,
                &self.context,
                &plan,
                current,
            )
            .unwrap()
        }

        fn registration(&self, client: Client) -> RegistrationState {
            let mut context = self.context.clone();
            context.exe = self.layout.exe.clone();
            setup::inspect(&context, client).registration
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_location_is_fixed_per_operating_system() {
        let dirs = BaseDirs {
            home: PathBuf::from("/home/ann"),
            local_app_data: Some(PathBuf::from("/c/Local")),
            app_data: Some(PathBuf::from("/c/Roaming")),
            data_home: None,
        };
        let windows = Layout::new(Platform::Windows, &dirs).unwrap();
        assert_eq!(windows.location, Path::new("/c/Local/Programs/CretLink"));
        assert_eq!(
            windows.exe,
            Path::new("/c/Local/Programs/CretLink/cretlink.exe")
        );
        assert_eq!(
            windows.record,
            Path::new("/c/Local/Programs/CretLink/install.json")
        );
        assert_eq!(
            windows.entry.as_deref(),
            Some(Path::new(
                "/c/Roaming/Microsoft/Windows/Start Menu/Programs/CretLink.lnk"
            ))
        );

        let mac = Layout::new(Platform::MacOs, &dirs).unwrap();
        assert_eq!(
            mac.location,
            Path::new("/home/ann/Applications/CretLink.app")
        );
        assert_eq!(
            mac.exe,
            Path::new("/home/ann/Applications/CretLink.app/Contents/MacOS/cretlink")
        );
        assert_eq!(mac.entry, None);

        let linux = Layout::new(Platform::Linux, &dirs).unwrap();
        assert_eq!(linux.exe, Path::new("/home/ann/.local/bin/cretlink"));
        assert_eq!(linux.cli, Path::new("/home/ann/.local/bin/cretlink-cli"));
        assert_eq!(
            linux.entry.as_deref(),
            Some(Path::new(
                "/home/ann/.local/share/applications/cretlink.desktop"
            ))
        );
        assert_eq!(
            linux.record,
            Path::new("/home/ann/.local/share/CretLink/install.json")
        );

        let windows_without_appdata = BaseDirs {
            local_app_data: None,
            ..dirs
        };
        assert_eq!(
            Layout::new(Platform::Windows, &windows_without_appdata),
            None
        );
    }

    #[test]
    fn install_copies_registers_and_records() {
        let fixture = Fixture::new("linux", Platform::Linux);
        let layout = &fixture.layout;
        let report = fixture
            .install(true, &[Client::Claude, Client::Gemini])
            .unwrap();
        assert!(!report.update);
        assert_eq!(report.registered, vec![Client::Claude, Client::Gemini]);
        assert!(report.failures.is_empty());
        assert_eq!(fs::read(&layout.exe).unwrap(), b"cretlink v1");
        assert_eq!(fs::read(&layout.cli).unwrap(), b"cli v1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&layout.exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        let desktop = fs::read_to_string(layout.entry.as_ref().unwrap()).unwrap();
        assert!(desktop.contains("Name=CretLink\n"), "{desktop}");
        // Quoted as `desktop_exec_arg` does: the Windows test runners' temporary
        // paths have backslashes, which the entry escapes.
        assert!(
            desktop.contains(&format!(
                "Exec={} --role technician\n",
                desktop_exec_arg(layout.exe.to_str().unwrap())
            )),
            "{desktop}"
        );

        // Both clients now start the installed program.
        for client in [Client::Claude, Client::Gemini] {
            assert!(
                matches!(
                    fixture.registration(client),
                    RegistrationState::ThisExecutable(_)
                ),
                "{client:?}"
            );
        }

        let record = read_record(layout).unwrap().unwrap();
        assert_eq!(record.schema, 1);
        assert_eq!(record.version, crate::RELEASE_VERSION);
        assert_eq!(record.files, vec![layout.exe.clone(), layout.cli.clone()]);
        assert_eq!(record.entry, layout.entry);
        assert_eq!(record.uninstall_entry, None);
        assert_eq!(record.clients(), vec![Client::Claude, Client::Gemini]);
        assert_eq!(
            record.registrations[0].command,
            layout.exe.to_string_lossy()
        );

        // No Windows-only step ran.
        assert!(fixture.system.shortcuts.borrow().is_empty());
        assert!(!fixture.system.has_uninstall_entry());
    }

    #[test]
    fn a_repeated_install_updates_in_place() {
        let fixture = Fixture::new("update", Platform::Linux);
        fixture.install(true, &[Client::Claude]).unwrap();
        write_file(&fixture.source(), b"cretlink v2", true).unwrap();
        // No cretlink-cli in the newer download: the older one goes.
        fs::remove_file(fixture.downloads.join("cretlink-cli")).unwrap();
        let calls_before = fixture.clients.calls.borrow().len();

        let report = fixture
            .install(false, &[Client::Claude, Client::Gemini])
            .unwrap();
        assert!(report.update);
        assert_eq!(fs::read(&fixture.layout.exe).unwrap(), b"cretlink v2");
        assert!(!fixture.layout.cli.exists());
        // The entry was not wanted again, so the earlier one is gone.
        assert!(!fixture.layout.entry.as_ref().unwrap().exists());
        // Claude Code already starts the installed path: nothing ran for it.
        let calls = fixture.clients.calls.borrow();
        assert_eq!(calls.len() - calls_before, 1, "{calls:?}");
        drop(calls);
        // The earlier registration stays in the record, once.
        let record = read_record(&fixture.layout).unwrap().unwrap();
        assert_eq!(record.clients(), vec![Client::Claude, Client::Gemini]);
        assert_eq!(record.entry, None);
    }

    #[test]
    fn uninstall_removes_what_the_record_lists_and_keeps_the_audit_trail() {
        let fixture = Fixture::new("uninstall", Platform::Linux);
        fixture
            .install(true, &[Client::Claude, Client::Gemini])
            .unwrap();
        // The record's folder also holds the audit trail, which stays.
        let audit = fixture.layout.record.parent().unwrap().join("audit");
        write_file(&audit.join("session.jsonl"), b"{}", false).unwrap();
        let elsewhere = fixture.downloads.join("cretlink");

        let plan = fixture.plan(&elsewhere);
        let lines = plan.lines();
        assert!(
            lines.contains(&fixture.layout.exe.display().to_string()),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("The desktop entry "))
        );
        assert!(lines.contains(
            &"The Claude Code registration: claude mcp remove --scope user cretlink".to_owned()
        ));
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("The Gemini CLI registration: gemini mcp remove"))
        );
        assert!(!plan.after_exit);
        assert_eq!(plan.audit_dir, audit_dir());

        let report = fixture.uninstall(&elsewhere);
        assert!(report.complete(), "{report:?}");
        assert!(report.pending.is_empty());
        assert!(!fixture.layout.exe.exists());
        assert!(!fixture.layout.cli.exists());
        assert!(!fixture.layout.record.exists());
        assert!(!fixture.layout.entry.as_ref().unwrap().exists());
        assert_eq!(
            fixture.registration(Client::Claude),
            RegistrationState::Absent
        );
        assert_eq!(
            fixture.registration(Client::Gemini),
            RegistrationState::Absent
        );
        assert!(audit.join("session.jsonl").exists());
        // A second uninstall finds nothing.
        assert!(matches!(
            uninstall_plan(
                &fixture.layout,
                &fixture.system,
                &fixture.context,
                &elsewhere
            ),
            Err(InstallError::NotInstalled(_))
        ));
    }

    /// Writes what Settings of the installed copy keeps: the settings file
    /// and the certificate file of its relay (ST-M007-011).
    fn keep_settings(layout: &Layout) {
        write_file(
            &layout.settings,
            b"[relay]\naddress = \"relay.example.org:443\"\nkey = \"abc\"\n",
            false,
        )
        .unwrap();
        write_file(&layout.relay_ca, b"-----BEGIN CERTIFICATE-----\n", false).unwrap();
    }

    #[test]
    fn uninstall_removes_the_settings_of_the_installed_copy() {
        let fixture = Fixture::new("settings", Platform::Linux);
        let layout = &fixture.layout;
        fixture.install(false, &[]).unwrap();
        keep_settings(layout);
        // A temporary file of an interrupted write would hold the key too.
        write_file(&temporary_name(&layout.settings), b"key", false).unwrap();
        let audit = layout.record.parent().unwrap().join("audit");
        write_file(&audit.join("session.jsonl"), b"{}", false).unwrap();
        let elsewhere = fixture.downloads.join("cretlink");

        let plan = fixture.plan(&elsewhere);
        assert_eq!(
            plan.settings,
            vec![
                layout.settings.clone(),
                temporary_name(&layout.settings),
                layout.relay_ca.clone()
            ]
        );
        // The settings are not listed a second time among the program files.
        assert_eq!(
            plan.files,
            vec![
                layout.exe.clone(),
                layout.cli.clone(),
                layout.record.clone()
            ]
        );
        let lines = plan.lines();
        assert!(
            lines.contains(&format!(
                "Your settings (the relay with its key, the network to wait on): {}",
                layout.settings.display()
            )),
            "{lines:?}"
        );
        assert!(
            lines.contains(&format!(
                "The certificate file of your relay: {}",
                layout.relay_ca.display()
            )),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("An unfinished copy of your settings: ")),
            "{lines:?}"
        );

        let report = fixture.uninstall(&elsewhere);
        assert!(report.complete(), "{report:?}");
        assert!(!layout.settings.exists());
        assert!(!layout.relay_ca.exists());
        assert!(!temporary_name(&layout.settings).exists());
        assert!(
            report
                .removed
                .contains(&layout.settings.display().to_string())
        );
        // The audit trail stays, in the same folder.
        assert!(audit.join("session.jsonl").exists());
    }

    #[test]
    fn uninstall_leaves_a_relay_profile_next_to_the_program_and_says_what_it_holds() {
        let fixture = Fixture::new("profile-kept", Platform::Linux);
        let layout = &fixture.layout;
        fixture.install(false, &[]).unwrap();
        let profile = relay_profile::path_next_to(&layout.exe).unwrap();
        let elsewhere = fixture.downloads.join("cretlink");
        let about_the_profile = |plan: &UninstallPlan| -> Vec<String> {
            plan.kept
                .iter()
                .filter(|line| line.contains("relay profile"))
                .cloned()
                .collect()
        };
        // Without such a file the plan says nothing about one.
        assert!(about_the_profile(&fixture.plan(&elsewhere)).is_empty());

        // A profile with a key: it stays, the plan says where it lies and
        // that it holds a key, and never repeats the key.
        let stays = format!(
            "The relay profile {}, which CretLink did not write and does not remove. ",
            profile.display()
        );
        write_file(
            &profile,
            b"[relay]\naddress = \"relay.example.org:443\"\nkey = \"abcabcabcabc\"\n",
            false,
        )
        .unwrap();
        let plan = fixture.plan(&elsewhere);
        assert_eq!(
            about_the_profile(&plan),
            [format!(
                "{stays}It holds the admission key of a relay in readable form: remove the file \
                 yourself when nobody else should be able to read that key"
            )]
        );
        assert!(!format!("{plan:?}").contains("abcabcabcabc"));
        // It is not among what is removed.
        assert!(!plan.files.contains(&profile));
        assert!(!plan.settings.contains(&profile));
        assert!(
            plan.lines()
                .iter()
                .all(|line| !line.contains(relay_profile::PROFILE_FILE)),
            "{:?}",
            plan.lines()
        );

        // Without a key, without a relay, and a file that cannot be read as
        // a profile: each is said as what it is.
        for (content, says) in [
            (
                &b"[relay]\naddress = \"relay.example.org:443\"\n"[..],
                "It holds the address of a relay and no admission key",
            ),
            (&b"# nothing yet\n"[..], "It holds no relay"),
            (
                &b"[relay]\naddress = \"relay.example.org:443\"\nkey = abcabcabcabc\n"[..],
                "It could not be read as a relay profile and may hold the admission key of a \
                 relay: look at it, and remove it yourself when nobody else should be able to \
                 read such a key",
            ),
        ] {
            write_file(&profile, content, false).unwrap();
            let plan = fixture.plan(&elsewhere);
            assert_eq!(about_the_profile(&plan), [format!("{stays}{says}")]);
            assert!(!format!("{plan:?}").contains("abcabcabcabc"));
        }

        // Uninstall leaves the file and reports it among what stays.
        let before = fs::read(&profile).unwrap();
        let report = fixture.uninstall(&elsewhere);
        assert!(report.complete(), "{report:?}");
        assert_eq!(fs::read(&profile).unwrap(), before);
        assert!(
            report
                .kept
                .iter()
                .any(|line| line.starts_with(&stays) && line.contains("may hold the admission key")),
            "{:?}",
            report.kept
        );
        assert!(!layout.exe.exists());
    }

    #[test]
    fn the_relay_profile_stays_on_windows_and_macos_too() {
        // Windows, uninstalling from the installed copy itself: the folder
        // goes once the program has closed, but only the files of the
        // install are removed from it, and the profile is none of them.
        let fixture = Fixture::new("profile-windows", Platform::Windows);
        let layout = &fixture.layout;
        fixture.install(false, &[]).unwrap();
        let profile = relay_profile::path_next_to(&layout.exe).unwrap();
        assert_eq!(profile.parent(), Some(layout.location.as_path()));
        write_file(
            &profile,
            b"[relay]\naddress = \"relay.example.org:443\"\nkey = \"abcabcabcabc\"\n",
            false,
        )
        .unwrap();
        let plan = fixture.plan(&layout.exe);
        assert!(plan.after_exit);
        assert!(
            plan.kept
                .iter()
                .any(|line| line.contains(&profile.display().to_string())
                    && line.contains("holds the admission key")),
            "{:?}",
            plan.kept
        );
        fixture.uninstall(&layout.exe);
        let scheduled = fixture.system.after_exit.borrow().clone();
        let (files, _dir) = &scheduled[0];
        assert!(!files.contains(&profile));
        assert!(profile.exists());

        // macOS: the program lies inside the application, and so would the
        // profile next to it. It is named all the same.
        let fixture = Fixture::new("profile-macos", Platform::MacOs);
        let layout = &fixture.layout;
        fixture.install(false, &[]).unwrap();
        let profile = relay_profile::path_next_to(&layout.exe).unwrap();
        write_file(
            &profile,
            b"[relay]\naddress = \"relay.example.org:443\"\n",
            false,
        )
        .unwrap();
        let current = fixture.downloads.join("cretlink");
        let plan = fixture.plan(&current);
        assert!(
            plan.kept
                .iter()
                .any(|line| line.contains(&profile.display().to_string())),
            "{:?}",
            plan.kept
        );
        assert!(fixture.uninstall(&current).complete());
        assert!(profile.exists());
    }

    #[test]
    fn settings_that_outlived_the_program_are_still_removed() {
        // The program files are gone already; the settings, with the relay's
        // key, are what is left of the install.
        let fixture = Fixture::new("settings-left", Platform::Linux);
        let layout = &fixture.layout;
        keep_settings(layout);
        let elsewhere = fixture.downloads.join("cretlink");
        let plan = fixture.plan(&elsewhere);
        assert!(plan.files.is_empty());
        assert!(!plan.is_empty());
        let report = fixture.uninstall(&elsewhere);
        assert!(report.complete(), "{report:?}");
        assert!(!layout.settings.exists());
        assert!(!layout.relay_ca.exists());
        assert!(matches!(
            uninstall_plan(layout, &fixture.system, &fixture.context, &elsewhere),
            Err(InstallError::NotInstalled(_))
        ));
    }

    #[test]
    fn an_update_in_place_keeps_the_settings() {
        let fixture = Fixture::new("settings-update", Platform::Windows);
        fixture.install(true, &[]).unwrap();
        keep_settings(&fixture.layout);
        write_file(&fixture.source(), b"cretlink v2", true).unwrap();
        assert!(fixture.install(true, &[]).unwrap().update);
        assert!(fixture.layout.settings.is_file());
        assert!(fixture.layout.relay_ca.is_file());
    }

    #[test]
    fn the_settings_go_with_the_folder_on_windows_and_with_the_bundle_on_macos() {
        // Windows, uninstalling from the installed copy itself: the settings
        // are among the files that go once it has closed.
        let fixture = Fixture::new("settings-windows", Platform::Windows);
        let layout = &fixture.layout;
        fixture.install(false, &[]).unwrap();
        keep_settings(layout);
        let plan = fixture.plan(&layout.exe);
        assert!(plan.after_exit);
        assert!(plan.lines().contains(&format!(
            "Your settings (the relay with its key, the network to wait on): {} \
             (removed once CretLink has closed)",
            layout.settings.display()
        )));
        fixture.uninstall(&layout.exe);
        let scheduled = fixture.system.after_exit.borrow().clone();
        let (files, dir) = &scheduled[0];
        assert!(files.contains(&layout.settings));
        assert!(files.contains(&layout.relay_ca));
        assert!(files.contains(&temporary_name(&layout.settings)));
        assert_eq!(dir, &layout.location);

        // macOS: they lie inside the application, and are named all the same.
        let fixture = Fixture::new("settings-macos", Platform::MacOs);
        let layout = &fixture.layout;
        fixture.install(false, &[]).unwrap();
        keep_settings(layout);
        let current = fixture.downloads.join("cretlink");
        let lines = fixture.plan(&current).lines();
        assert_eq!(
            lines[0],
            format!("The application {}", layout.location.display())
        );
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(fixture.uninstall(&current).complete());
        assert!(!layout.location.exists());
    }

    #[test]
    fn a_registration_changed_by_hand_is_left_alone() {
        let fixture = Fixture::new("changed", Platform::Linux);
        fixture
            .install(false, &[Client::Claude, Client::Gemini])
            .unwrap();
        // The technician points Claude Code at another build by hand.
        write_file(
            &fixture.root.join("home").join(".claude.json"),
            br#"{"mcpServers": {"cretlink": {"command": "/opt/other/cretlink", "args": ["mcp"]}}}"#,
            false,
        )
        .unwrap();
        let current = fixture.downloads.join("cretlink");
        let plan = fixture.plan(&current);
        assert_eq!(plan.registrations.len(), 1);
        assert_eq!(plan.registrations[0].client, Client::Gemini);
        assert!(
            plan.kept.iter().any(
                |line| line.contains("Claude Code") && line.contains("/opt/other/cretlink mcp")
            ),
            "{:?}",
            plan.kept
        );
        let calls_before = fixture.clients.calls.borrow().len();
        let report = fixture.uninstall(&current);
        assert!(report.complete());
        let calls = fixture.clients.calls.borrow();
        assert_eq!(
            calls[calls_before..].to_vec(),
            vec![vec![
                "mcp".to_owned(),
                "remove".to_owned(),
                "--scope".to_owned(),
                "user".to_owned(),
                "cretlink".to_owned()
            ]]
        );
        drop(calls);
        assert!(matches!(
            fixture.registration(Client::Claude),
            RegistrationState::OtherCommand(_)
        ));
    }

    #[test]
    fn a_running_installed_copy_stops_install_and_uninstall() {
        let fixture = Fixture::new("running", Platform::Linux);
        fixture.install(false, &[]).unwrap();
        fixture
            .system
            .running
            .borrow_mut()
            .push(fixture.layout.exe.clone());
        write_file(&fixture.source(), b"cretlink v2", true).unwrap();
        let err = fixture.install(false, &[]).unwrap_err();
        assert!(matches!(err, InstallError::Running(_)), "{err}");
        assert!(err.to_string().contains("is running"));
        // Nothing was replaced.
        assert_eq!(fs::read(&fixture.layout.exe).unwrap(), b"cretlink v1");

        let current = fixture.downloads.join("cretlink");
        let plan = fixture.plan(&current);
        let err = uninstall(
            &fixture.layout,
            &fixture.system,
            &fixture.clients,
            &fixture.context,
            &plan,
            &current,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::Running(_)));
        assert!(fixture.layout.exe.exists());
    }

    #[test]
    fn a_customer_copy_refuses_install_and_uninstall() {
        let fixture = Fixture::new("customer", Platform::Linux);
        let customer = fixture.downloads.join("CretLink-Support (1)");
        write_file(&customer, b"cretlink v1", true).unwrap();
        let err = install(
            &fixture.layout,
            &fixture.system,
            &fixture.clients,
            &fixture.context,
            &InstallRequest {
                source: customer.clone(),
                entry: true,
                clients: vec![Client::Claude],
            },
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::CustomerCopy));
        assert!(!fixture.layout.exe.exists());
        assert!(fixture.clients.calls.borrow().is_empty());

        fixture.install(false, &[]).unwrap();
        assert!(matches!(
            uninstall_plan(
                &fixture.layout,
                &fixture.system,
                &fixture.context,
                &customer
            ),
            Err(InstallError::CustomerCopy)
        ));
        let plan = fixture.plan(&fixture.downloads.join("cretlink"));
        assert!(matches!(
            uninstall(
                &fixture.layout,
                &fixture.system,
                &fixture.clients,
                &fixture.context,
                &plan,
                &customer
            ),
            Err(InstallError::CustomerCopy)
        ));
        assert!(fixture.layout.exe.exists());
        assert!(is_customer_copy(Path::new("C:/x/cretlink-support.exe")));
        assert!(!is_customer_copy(Path::new("/x/cretlink")));
    }

    #[test]
    fn the_installed_copy_does_not_install_again() {
        let fixture = Fixture::new("again", Platform::Linux);
        fixture.install(false, &[]).unwrap();
        let err = install(
            &fixture.layout,
            &fixture.system,
            &fixture.clients,
            &fixture.context,
            &InstallRequest {
                source: fixture.layout.exe.clone(),
                entry: false,
                clients: vec![],
            },
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::AlreadyInstalled(_)));
        assert!(fixture.layout.is_installed_exe(&fixture.layout.exe));
        assert!(!fixture.layout.is_installed_exe(&fixture.source()));
    }

    #[test]
    fn windows_adds_a_shortcut_and_an_uninstall_entry_and_removes_its_folder_after_exit() {
        let fixture = Fixture::new("windows", Platform::Windows);
        let layout = &fixture.layout;
        fixture.install(true, &[Client::Claude]).unwrap();
        assert!(layout.exe.ends_with("Programs/CretLink/cretlink.exe"));
        let shortcuts = fixture.system.shortcuts.borrow().clone();
        assert_eq!(
            shortcuts,
            vec![Shortcut {
                path: layout.entry.clone().unwrap(),
                target: layout.exe.clone(),
                arguments: "--role technician".to_owned(),
                description: "CretLink: give help".to_owned(),
            }]
        );
        let entry = fixture.system.entry.borrow().clone().unwrap();
        assert_eq!(entry.display_name, "CretLink");
        assert_eq!(entry.display_version, crate::RELEASE_VERSION);
        assert_eq!(entry.publisher, PUBLISHER);
        assert_eq!(entry.install_location, layout.location.to_string_lossy());
        assert_eq!(
            entry.uninstall_string,
            format!("\"{}\" uninstall", layout.exe.display())
        );
        let record = read_record(layout).unwrap().unwrap();
        assert_eq!(
            record.uninstall_entry.as_deref(),
            Some(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\CretLink")
        );

        // Uninstalling from the installed copy itself: the folder goes after
        // the process exits.
        let plan = fixture.plan(&layout.exe);
        assert!(plan.after_exit);
        let lines = plan.lines();
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("(removed once CretLink has closed)"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("The Start-menu shortcut "))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with(r"The uninstall entry HKCU\"))
        );
        let report = fixture.uninstall(&layout.exe);
        assert!(report.complete(), "{report:?}");
        assert!(!fixture.system.has_uninstall_entry());
        assert!(!layout.entry.as_ref().unwrap().exists());
        assert_eq!(
            fixture.registration(Client::Claude),
            RegistrationState::Absent
        );
        let scheduled = fixture.system.after_exit.borrow().clone();
        assert_eq!(scheduled, vec![(layout.files(), layout.location.clone())]);
        // The files are left for the scheduled removal.
        assert!(layout.exe.exists());
        assert_eq!(report.pending.len(), 1);
    }

    #[test]
    fn windows_uninstall_from_the_installed_cli_also_waits_for_its_exit() {
        // `cretlink-cli uninstall` run from the install folder cannot delete
        // its own file either: the folder goes after it exits.
        let fixture = Fixture::new("windows-cli", Platform::Windows);
        let layout = &fixture.layout;
        fixture.install(false, &[]).unwrap();
        assert!(layout.runs_from_install(&layout.cli));
        assert!(layout.runs_from_install(&layout.exe));
        assert!(!layout.runs_from_install(&fixture.source()));
        let plan = fixture.plan(&layout.cli);
        assert!(plan.after_exit);
        let report = fixture.uninstall(&layout.cli);
        assert!(report.complete(), "{report:?}");
        let scheduled = fixture.system.after_exit.borrow().clone();
        assert_eq!(scheduled, vec![(layout.files(), layout.location.clone())]);
        assert_eq!(report.pending.len(), 1);
        // From a copy elsewhere the files go at once.
        assert!(!fixture.plan(&fixture.source()).after_exit);
    }

    #[test]
    fn macos_generates_a_minimal_bundle() {
        let fixture = Fixture::new("macos", Platform::MacOs);
        let layout = &fixture.layout;
        fixture.install(true, &[]).unwrap();
        let plist = fs::read_to_string(layout.info_plist.as_ref().unwrap()).unwrap();
        assert!(plist.contains("<key>CFBundleExecutable</key>\n\t<string>cretlink</string>"));
        assert!(plist.contains(&format!("<string>{}</string>", crate::RELEASE_VERSION)));
        assert!(plist.contains(BUNDLE_ID));
        assert!(layout.exe.is_file());
        assert!(layout.record.is_file());

        let current = fixture.downloads.join("cretlink");
        assert_eq!(
            fixture.plan(&current).lines(),
            vec![format!("The application {}", layout.location.display())]
        );
        let report = fixture.uninstall(&current);
        assert!(report.complete());
        assert!(!layout.location.exists());
        assert!(layout.location.parent().unwrap().exists());
    }

    #[test]
    fn an_unreadable_record_keeps_the_registrations_and_says_so() {
        let fixture = Fixture::new("bad-record", Platform::Linux);
        fixture.install(false, &[Client::Claude]).unwrap();
        write_file(&fixture.layout.record, b"{ not json", false).unwrap();
        let current = fixture.downloads.join("cretlink");
        let plan = fixture.plan(&current);
        assert!(plan.registrations.is_empty());
        assert!(
            plan.kept[0].contains("could not be read"),
            "{:?}",
            plan.kept
        );
        let report = fixture.uninstall(&current);
        assert!(report.complete());
        assert!(!fixture.layout.exe.exists());
        assert!(!fixture.layout.record.exists());
        assert!(matches!(
            fixture.registration(Client::Claude),
            RegistrationState::ThisExecutable(_)
        ));
    }

    #[test]
    fn uninstall_never_deletes_a_path_read_from_the_record() {
        let fixture = Fixture::new("tampered", Platform::Linux);
        fixture.install(false, &[]).unwrap();
        let victim = fixture.root.join("victim");
        write_file(&victim, b"keep", false).unwrap();
        let mut record = read_record(&fixture.layout).unwrap().unwrap();
        record.files.push(victim.clone());
        record.location = fixture.root.clone();
        write_record(&fixture.layout, &record).unwrap();
        let report = fixture.uninstall(&fixture.downloads.join("cretlink"));
        assert!(report.complete());
        assert!(victim.exists());
        assert!(!fixture.layout.exe.exists());
    }

    #[test]
    fn a_registration_made_later_is_recorded() {
        let fixture = Fixture::new("record-later", Platform::Linux);
        // Nothing installed: nothing to record.
        record_registration(&fixture.layout, Client::Gemini).unwrap();
        assert!(!fixture.layout.record.exists());
        fixture.install(false, &[]).unwrap();
        record_registration(&fixture.layout, Client::Gemini).unwrap();
        record_registration(&fixture.layout, Client::Gemini).unwrap();
        let record = read_record(&fixture.layout).unwrap().unwrap();
        assert_eq!(record.clients(), vec![Client::Gemini]);
    }

    #[test]
    fn the_windows_removal_waits_for_this_process_and_quotes_paths() {
        let script = removal_script(
            42,
            &[
                PathBuf::from(r"C:\Users\Ann O'Neil\cretlink.exe"),
                PathBuf::from("C:\\x\u{2019}y\\install.json"),
            ],
            Path::new(r"C:\Users\Ann O'Neil"),
        );
        assert!(script.starts_with("Wait-Process -Id 42 "), "{script}");
        // No time limit: removing while the program still runs would fail.
        assert!(!script.contains("-Timeout"), "{script}");
        assert!(
            script.contains(
                "-LiteralPath 'C:\\Users\\Ann O''Neil\\cretlink.exe','C:\\x\u{2019}\u{2019}y\\install.json' -Force"
            ),
            "{script}"
        );
        assert!(
            script.ends_with(r"Remove-Item -LiteralPath 'C:\Users\Ann O''Neil' -Force }"),
            "{script}"
        );
        assert!(!script.contains('"'));
    }

    #[test]
    fn desktop_exec_arguments_are_quoted() {
        assert_eq!(desktop_exec_arg("/home/a b/x"), "\"/home/a b/x\"");
        assert_eq!(desktop_exec_arg("/x/100%"), "\"/x/100%%\"");
        assert_eq!(desktop_exec_arg("/x/$y\"z"), "\"/x/\\\\$y\\\\\"z\"");
        assert!(desktop_entry(Path::new("/x/a\nb")).is_err());
    }
}
