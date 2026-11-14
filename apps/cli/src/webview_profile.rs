//! The webview's profile folder: where it is, and that it goes again
//! (ST-M009-014).
//!
//! The application's window is a webview, and a webview keeps a profile — its
//! cache and local storage, no session secret — in a folder. Left to itself,
//! that folder is named after the application identifier in the per-user data
//! folder ([`LEGACY_DIR`]) and stays after the window has closed. So every
//! window is told where its profile goes ([`Home`]):
//!
//! | Process | Profile folder | Removed |
//! |---|---|---|
//! | Not installed (the start screen, a technician that runs once, a `CretLink-Support*` copy) and every customer process | `CretLink-webview-<random>` in the temporary folder ([`TempProfile`]) | by the process, after its last window closed |
//! | The installed technician copy | `webview` in the application's data folder, next to the audit trail ([`installed_dir`]) | by uninstall |
//! | Any process on macOS | none: WebKit keeps its data where a program cannot direct it | not by CretLink |
//!
//! # The lock, and folders that were left
//!
//! A process cannot always remove its temporary folder: it may be killed, or
//! the webview may still hold files when the process ends. So each process
//! holds a lock on [`LOCK_FILE`] in its folder for as long as it runs; the
//! operating system gives the lock back when the process ends, however it
//! ends. A folder whose lock can be taken belongs to no running process:
//! every start of a CretLink window removes those ([`sweep`]), `recover` lists
//! them ([`leftovers`]) and `recover --remove` removes them ([`recover`]). A
//! folder whose lock is held is never touched, and a folder whose lock cannot
//! be told is left alone and named.
//!
//! What keeps this right when two processes start at the same moment:
//!
//! - A process only ever uses a folder it created itself, new, under a random
//!   name ([`TempProfile::create_in`]). A name that is taken, by whatever
//!   stands there, is never taken over: another name is drawn. The process
//!   creates the folder, then the lock file, then takes the lock, and gives
//!   the folder up when one of these steps fails or when the lock file is gone
//!   afterwards. Nothing else is put into the folder before that.
//! - Only that process creates a lock file. A sweep opens the one that exists,
//!   and removes a folder only while it holds its lock; a creation that runs
//!   at that moment finds the lock taken and draws another name.
//! - A folder without a lock file is either being created, and then empty, or
//!   what an unfinished removal left. An empty one is removed in one step,
//!   which a creation in progress notices. One with content is removed only
//!   when the lock file is still missing after the content was seen: content
//!   comes after the lock, so nobody is creating it.
//!
//! # Links are never followed
//!
//! The temporary folder is one that other programs of the same account can
//! write, and the customer role runs with administrator rights (REQ-S-024).
//! Only real folders with a generated name, directly inside the temporary
//! folder, are looked at: a link under such a name is not one. A lock is
//! opened only where a file stands under its name, and nothing is read from
//! it. A folder is removed whole through [`cretlink_exec::remove_whole`],
//! which removes a link as a link and never enters what it points at.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cretlink_exec::{Standing, remove_whole};

use crate::install::{BaseDirs, Platform};

/// What the name of every temporary profile folder starts with.
pub const TEMP_PREFIX: &str = "CretLink-webview-";

/// The file in a temporary profile folder that its process keeps locked.
pub const LOCK_FILE: &str = "cretlink.lock";

/// The installed copy's profile folder, in the application's data folder.
pub const INSTALLED_DIR: &str = "webview";

/// The application's data folder, which also holds the audit trail.
const DATA_DIR: &str = "CretLink";

/// The folder earlier versions left in the per-user data folder: the
/// webview's default, named after the application identifier.
pub const LEGACY_DIR: &str = "dev.cretlink.app";

/// The hexadecimal digits that end the name of a temporary profile folder.
const RANDOM_DIGITS: usize = 32;

/// How many names a creation draws before it gives up. A name is lost only
/// when it is taken, or when another copy's sweep meets the folder in the
/// moment it is created.
const CREATE_ATTEMPTS: usize = 8;

// --- How long a removal keeps trying ---

/// How long removals keep trying while a webview that is closing still holds
/// files in its folder: a number of pauses, shared by every folder that one
/// operation removes, so that the whole operation ends in a few seconds.
#[derive(Debug)]
pub struct Patience {
    pauses_left: u32,
    pause: Duration,
}

impl Patience {
    /// A few seconds: fifty pauses of a tenth of a second.
    #[must_use]
    pub const fn a_few_seconds() -> Self {
        Self::of(50, Duration::from_millis(100))
    }

    /// None: every removal is tried once.
    #[must_use]
    pub const fn none() -> Self {
        Self::of(0, Duration::ZERO)
    }

    /// `pauses` pauses of `pause` each.
    #[must_use]
    pub const fn of(pauses: u32, pause: Duration) -> Self {
        Self {
            pauses_left: pauses,
            pause,
        }
    }

    /// Pauses before another try. `false` when no try is left.
    fn pause(&mut self) -> bool {
        if self.pauses_left == 0 {
            return false;
        }
        self.pauses_left -= 1;
        std::thread::sleep(self.pause);
        true
    }
}

// --- Where the profile goes ---

/// The per-user data folder the application's own folders are in:
/// `%LOCALAPPDATA%` on Windows, `$XDG_DATA_HOME` or `~/.local/share` on Linux.
/// `None` on macOS, where the webview takes no folder, and on Windows without
/// `%LOCALAPPDATA%`.
fn data_home(platform: Platform, dirs: &BaseDirs) -> Option<PathBuf> {
    match platform {
        Platform::Windows => dirs.local_app_data.clone(),
        Platform::Linux => Some(
            dirs.data_home
                .clone()
                .unwrap_or_else(|| dirs.home.join(".local").join("share")),
        ),
        Platform::MacOs => None,
    }
}

/// The installed copy's profile folder: `%LOCALAPPDATA%\CretLink\webview`,
/// `~/.local/share/CretLink/webview`. It is next to the audit trail, which
/// uninstall keeps.
#[must_use]
pub fn installed_dir(platform: Platform, dirs: &BaseDirs) -> Option<PathBuf> {
    data_home(platform, dirs).map(|data| data.join(DATA_DIR).join(INSTALLED_DIR))
}

/// The profile folder earlier versions created for this user:
/// `%LOCALAPPDATA%\dev.cretlink.app`, `~/.local/share/dev.cretlink.app`.
#[must_use]
pub fn legacy_dir(platform: Platform, dirs: &BaseDirs) -> Option<PathBuf> {
    data_home(platform, dirs).map(|data| data.join(LEGACY_DIR))
}

/// Where a process's webview keeps its profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Home {
    /// A folder of the process's own in the temporary folder
    /// ([`TempProfile`]), removed when the process ends.
    Temporary,
    /// The installed copy's folder ([`installed_dir`]), which uninstall
    /// removes.
    Installed(PathBuf),
    /// Nowhere CretLink decides: on macOS the webview takes no folder.
    Unmanaged,
}

impl Home {
    /// Decides the profile of a process. `installed` is the profile folder of
    /// the installed copy when this process runs from the install location
    /// ([`crate::install::Layout::webview`]), and `customer` is whether the
    /// process shows the customer role.
    ///
    /// Only the installed copy keeps a profile, and only outside the customer
    /// role: Get help leaves nothing behind wherever it was started from, and
    /// its elevated process never shares a folder with one that is not.
    #[must_use]
    pub fn choose(platform: Platform, installed: Option<&Path>, customer: bool) -> Self {
        if platform == Platform::MacOs {
            return Self::Unmanaged;
        }
        match installed {
            Some(dir) if !customer => Self::Installed(dir.to_path_buf()),
            _ => Self::Temporary,
        }
    }
}

// --- A process's own temporary profile ---

/// A temporary profile folder this process created, and holds the lock of.
#[derive(Debug)]
pub struct TempProfile {
    path: PathBuf,
    /// Locked for as long as this value lives: for the life of the process.
    lock: File,
}

impl TempProfile {
    /// Creates `CretLink-webview-<random>` in `temp`, new and for this user
    /// only where the platform has file modes, and takes its lock. The name
    /// comes from the system's random source, and a name under which
    /// something stands already is never taken over.
    ///
    /// # Errors
    ///
    /// Returns the last error when no folder could be created and locked:
    /// `temp` does not exist or cannot be written.
    pub fn create_in(temp: &Path) -> io::Result<Self> {
        let mut last = io::Error::other("no folder name was tried");
        for _ in 0..CREATE_ATTEMPTS {
            let path = temp.join(profile_name(rand::random::<u128>()));
            match Self::create_at(&path) {
                Ok(profile) => return Ok(profile),
                Err(err) => last = err,
            }
        }
        Err(last)
    }

    /// Creates the folder at `path`, where nothing may stand, and takes its
    /// lock.
    fn create_at(path: &Path) -> io::Result<Self> {
        create_private_dir(path)?;
        match Self::claim(path) {
            Ok(lock) => Ok(Self {
                path: path.to_path_buf(),
                lock,
            }),
            Err(err) => {
                // Given up. A sweep that took it for a folder that was left
                // removes it; when it is still empty, so does this.
                let _ = fs::remove_dir(path);
                Err(err)
            }
        }
    }

    /// Creates and locks the lock file of the new, empty folder at `path`.
    fn claim(path: &Path) -> io::Result<File> {
        let lock_path = path.join(LOCK_FILE);
        let lock = create_lock(&lock_path)?;
        match Self::hold(&lock, &lock_path) {
            Ok(()) => Ok(lock),
            Err(err) => {
                // Given up: this process made the file, so it takes it away
                // again, closed first so that it can go.
                drop(lock);
                let _ = fs::remove_file(&lock_path);
                Err(err)
            }
        }
    }

    /// Takes the lock of the file this process just created at `lock_path`.
    fn hold(lock: &File, lock_path: &Path) -> io::Result<()> {
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "the new folder was taken for one that was left",
                ));
            }
            // A file system without locks. No process can take this lock
            // either, so none takes the folder for one that was left: it is
            // used without a lock, and removed by this process only.
            Err(TryLockError::Error(err)) if err.kind() == io::ErrorKind::Unsupported => {}
            Err(TryLockError::Error(err)) => return Err(err),
        }
        // A sweep that held the lock a moment ago has removed the file, and
        // the folder with it: the lock taken above is then on nothing.
        if Standing::of_file(lock_path) != Standing::Ours {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the new folder was removed while it was created",
            ));
        }
        Ok(())
    }

    /// The folder, which every window of this process gives its webview.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Gives the lock back and removes the folder whole, trying again for
    /// `patience` while a webview that is closing still holds files in it.
    /// Call it at the end of the process, after the last window has closed.
    ///
    /// # Errors
    ///
    /// Returns the last error when the folder is still there afterwards. Its
    /// lock is free then, so the next start of a CretLink window removes it,
    /// and `recover` lists it.
    pub fn remove(self, patience: &mut Patience) -> io::Result<()> {
        let Self { path, lock } = self;
        // Given back first: where an open file keeps its place in its folder
        // until it is closed (Windows), the folder could not go otherwise.
        drop(lock);
        remove_tree(&path, patience)
    }
}

/// The name of the temporary profile folder that ends in `random`.
fn profile_name(random: u128) -> String {
    format!("{TEMP_PREFIX}{random:032x}")
}

/// Whether `name` is a temporary profile folder's: the prefix and the random
/// digits, exactly as [`TempProfile::create_in`] makes them.
fn is_profile_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(TEMP_PREFIX))
        .is_some_and(|digits| {
            digits.len() == RANDOM_DIGITS
                && digits
                    .bytes()
                    .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
        })
}

/// Creates one new folder, for this user only where the platform has file
/// modes (0700). On Windows the new folder takes the permissions that the
/// temporary folder hands down. Fails when anything stands at `path`.
fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

/// Creates the installed copy's profile folder when it is not there yet,
/// with the application's data folder it is in; new folders are for this
/// user only where the platform has file modes (0700).
///
/// # Errors
///
/// Returns an error when the folder cannot be created.
pub fn create_installed(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// Creates the lock file, where nothing may stand.
fn create_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Shared for reading and writing, not for deleting: while the process
        // holds the file, nothing removes or renames it, so the folder of a
        // running copy never looks like one without a lock.
        const SHARE_READ_AND_WRITE: u32 = 0x0000_0001 | 0x0000_0002;
        options.share_mode(SHARE_READ_AND_WRITE);
    }
    options.open(path)
}

// --- Folders other processes left ---

/// The lock of a profile folder, seen by a process that did not create it.
#[derive(Debug)]
enum Lock {
    /// Free, and now held through this file: no process runs with the folder.
    Free(File),
    /// Held: the folder's process runs.
    Held,
    /// There is no lock file.
    Missing,
    /// It cannot be told, and why: something that is no file stands under
    /// the lock's name, or the file cannot be opened or locked.
    Unknown(String),
}

/// Opens the lock file of `dir`, never creating it, and tries to take the
/// lock. Nothing is read from the file.
fn probe(dir: &Path) -> Lock {
    let path = dir.join(LOCK_FILE);
    match Standing::of_file(&path) {
        Standing::Absent => return Lock::Missing,
        Standing::Foreign => {
            return Lock::Unknown("something that is no file stands where its lock belongs".into());
        }
        Standing::Ours => {}
    }
    let mut options = OpenOptions::new();
    // Read and write, so the open never waits on something that is not a
    // regular file.
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // What stands under the name is opened itself: a link that took the
        // file's place since the look above is not followed.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(OPEN_REPARSE_POINT);
    }
    match options.open(&path) {
        Ok(file) => match file.try_lock() {
            Ok(()) => Lock::Free(file),
            Err(TryLockError::WouldBlock) => Lock::Held,
            Err(TryLockError::Error(err)) => {
                Lock::Unknown(format!("its lock could not be tried ({})", err.kind()))
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => Lock::Missing,
        Err(err) => Lock::Unknown(format!("its lock could not be opened ({})", err.kind())),
    }
}

/// The temporary profile folders in `temp`: real folders — a link is not one
/// — directly inside it, with a generated name. A `temp` that does not exist
/// holds none.
fn candidates(temp: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(temp) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| is_profile_name(&entry.file_name()))
        .map(|entry| entry.path())
        // The entry's own kind: a link is not followed.
        .filter(|path| Standing::of_dir(path) == Standing::Ours)
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Whether nothing stands at `path` (any more).
fn is_gone(path: &Path) -> bool {
    Standing::of_dir(path) == Standing::Absent
}

/// Whether `dir` holds anything but a lock file.
fn has_content(dir: &Path) -> io::Result<bool> {
    Ok(fs::read_dir(dir)?
        .flatten()
        .any(|entry| entry.file_name() != LOCK_FILE))
}

/// Removes what stands at `path` whole, without following a link
/// ([`cretlink_exec::remove_whole`]), trying again for `patience`: a webview
/// that is closing holds files in its profile for a moment longer. A path
/// where nothing stands is removed already.
///
/// # Errors
///
/// Returns the last error when something is still there afterwards.
pub fn remove_tree(path: &Path, patience: &mut Patience) -> io::Result<()> {
    remove_with(path, patience, &mut |path| remove_whole(path))
}

/// [`remove_tree`] with the removal itself given, for tests.
fn remove_with(
    path: &Path,
    patience: &mut Patience,
    remove: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    loop {
        let err = match remove(path) {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };
        // Gone, whoever removed it.
        if is_gone(path) {
            return Ok(());
        }
        if !patience.pause() {
            return Err(err);
        }
    }
}

/// Removes the installed copy's profile and, when nothing else is in it, the
/// application's data folder it is in. The audit trail keeps that folder.
///
/// # Errors
///
/// As [`remove_tree`].
pub fn remove_installed(profile: &Path, patience: &mut Patience) -> io::Result<()> {
    remove_tree(profile, patience)?;
    if let Some(data) = profile.parent() {
        // Only when empty.
        let _ = fs::remove_dir(data);
    }
    Ok(())
}

/// Removes the folder `dir`, which no process runs with: `lock` is its lock,
/// held, or `None` when it has no lock file. `Ok(false)` when it turned out to
/// be in use after all — a process was creating it — and was left alone.
fn remove_left(dir: &Path, lock: Option<File>, patience: &mut Patience) -> io::Result<bool> {
    let Some(lock) = lock else {
        return remove_unlocked(dir, patience);
    };
    // The lock stays held for the first try: a process that is creating this
    // folder at this very moment finds it taken, and draws another name.
    let first = remove_whole(dir);
    drop(lock);
    match first {
        Ok(()) => Ok(true),
        // What is left is this process's own handle on the lock file (it kept
        // its place in the folder until now), or files that a closing webview
        // still holds.
        Err(_) => remove_tree(dir, patience).map(|()| true),
    }
}

/// Removes a folder that has no lock file: one that is being created is
/// empty, and removing an empty folder is one step, which the creation
/// notices; one with content is what an unfinished removal left.
fn remove_unlocked(dir: &Path, patience: &mut Patience) -> io::Result<bool> {
    let not_removed = match fs::remove_dir(dir) {
        Ok(()) => return Ok(true),
        Err(err) => err,
    };
    if is_gone(dir) {
        return Ok(true);
    }
    // First the content, then the lock once more: content comes after the
    // lock, so content without a lock file is not a creation in progress.
    let content = match has_content(dir) {
        Ok(content) => content,
        Err(_) if is_gone(dir) => return Ok(true),
        Err(err) => return Err(err),
    };
    match probe(dir) {
        Lock::Missing if content => remove_tree(dir, patience).map(|()| true),
        // Empty, and yet it could not be removed.
        Lock::Missing => Err(not_removed),
        Lock::Free(lock) => remove_left(dir, Some(lock), patience),
        // A process got there first: the folder is its own now.
        Lock::Held | Lock::Unknown(_) => Ok(false),
    }
}

/// What a look at the temporary folder found.
#[derive(Debug, Default)]
struct Found {
    /// The folders that no process runs with, each with its lock, held, when
    /// it has a lock file.
    left: Vec<(PathBuf, Option<File>)>,
    /// The folders whose lock cannot be told: left alone, and named.
    unknown: Vec<Untold>,
}

/// Looks at the temporary profile folders in `temp`, skipping `own`. Each
/// lock that could be taken is held until the value is dropped.
fn look(temp: &Path, own: Option<&Path>) -> io::Result<Found> {
    let mut found = Found::default();
    for dir in candidates(temp)? {
        // By its name: the two paths may spell the temporary folder
        // differently.
        if own.is_some_and(|own| own.file_name() == dir.file_name()) {
            continue;
        }
        match probe(&dir) {
            Lock::Free(lock) => found.left.push((dir, Some(lock))),
            Lock::Missing => found.left.push((dir, None)),
            Lock::Held => {}
            Lock::Unknown(reason) => found.unknown.push(Untold { path: dir, reason }),
        }
    }
    Ok(found)
}

/// A folder that could not be removed, or whose lock cannot be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untold {
    /// The folder.
    pub path: PathBuf,
    /// Why it is still there, or why nothing is known about it.
    pub reason: String,
}

/// What a sweep of the temporary folder did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    /// The folders it removed.
    pub removed: Vec<PathBuf>,
    /// The folders it could not remove. They are left for a later start.
    pub kept: Vec<Untold>,
    /// The folders whose lock cannot be told. They were left alone.
    pub unknown: Vec<Untold>,
}

/// Removes the temporary profile folders in `temp` that no process runs with,
/// as every start of a CretLink window does. `own` is this process's folder,
/// which is skipped whatever its lock says. A folder whose lock is held is
/// never touched, one whose lock cannot be told is left alone and named, and
/// one that cannot be removed is left where it is, after trying for
/// `patience`.
///
/// # Errors
///
/// Returns an error when `temp` cannot be listed. Nothing was removed then.
pub fn sweep(temp: &Path, own: Option<&Path>, patience: &mut Patience) -> io::Result<Sweep> {
    let found = look(temp, own)?;
    let mut sweep = Sweep {
        unknown: found.unknown,
        ..Sweep::default()
    };
    for (dir, lock) in found.left {
        match remove_left(&dir, lock, patience) {
            Ok(true) => sweep.removed.push(dir),
            Ok(false) => {}
            Err(err) => sweep.kept.push(Untold {
                path: dir,
                reason: err.to_string(),
            }),
        }
    }
    Ok(sweep)
}

// --- What `recover` lists and removes ---

/// Why a webview folder is a leftover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A temporary profile whose process no longer runs.
    Stale,
    /// The profile folder of an earlier version ([`LEGACY_DIR`]).
    EarlierVersion,
}

/// A webview folder CretLink left on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    /// The folder.
    pub path: PathBuf,
    /// Why it is one.
    pub origin: Origin,
}

impl Leftover {
    /// The folder and where it comes from, for a report.
    #[must_use]
    pub fn describe(&self) -> String {
        let origin = match self.origin {
            Origin::Stale => "left by a copy that no longer runs",
            Origin::EarlierVersion => "left by an earlier version",
        };
        format!("{} ({origin})", self.path.display())
    }
}

/// The places `recover` looks for webview folders in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    /// The temporary folder of this process.
    pub temp: PathBuf,
    /// Where earlier versions kept the profile of this user ([`legacy_dir`]).
    pub legacy: Option<PathBuf>,
}

impl Places {
    /// The places for `platform` and `dirs`, with `temp` as the temporary
    /// folder.
    #[must_use]
    pub fn new(platform: Platform, dirs: Option<&BaseDirs>, temp: PathBuf) -> Self {
        Self {
            temp,
            legacy: dirs.and_then(|dirs| legacy_dir(platform, dirs)),
        }
    }

    /// The places of this computer and user.
    #[must_use]
    pub fn current() -> Self {
        Self::new(
            Platform::current(),
            BaseDirs::from_env().as_ref(),
            std::env::temp_dir(),
        )
    }

    /// The folder of an earlier version, when something stands under its
    /// name. Whatever stands there is removed whole and never entered.
    fn earlier_version(&self) -> Option<Leftover> {
        let path = self.legacy.as_ref().filter(|path| !is_gone(path))?;
        Some(Leftover {
            path: path.clone(),
            origin: Origin::EarlierVersion,
        })
    }
}

/// The webview folders found, for `recover`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Leftovers {
    /// The folders that were left. A folder whose process runs is not one.
    pub found: Vec<Leftover>,
    /// The folders whose lock cannot be told: it is not known whether a
    /// process runs with them.
    pub unknown: Vec<Untold>,
    /// Why the temporary folder could not be listed, when it could not: what
    /// it holds is then unknown, not absent.
    pub unreadable: Option<String>,
}

/// Lists the webview folders CretLink left, removing nothing.
#[must_use]
pub fn leftovers(places: &Places) -> Leftovers {
    let mut leftovers = Leftovers::default();
    match look(&places.temp, None) {
        Ok(found) => {
            leftovers
                .found
                .extend(found.left.into_iter().map(|(path, _lock)| Leftover {
                    path,
                    origin: Origin::Stale,
                }));
            leftovers.unknown = found.unknown;
        }
        Err(err) => leftovers.unreadable = Some(unreadable(&places.temp, &err)),
    }
    leftovers.found.extend(places.earlier_version());
    leftovers
}

/// What `recover --remove` did with the webview folders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// The folders that were left, before anything was removed.
    pub found: Vec<Leftover>,
    /// The folders removed.
    pub removed: Vec<Leftover>,
    /// The folders that could not be removed, with the reason.
    pub kept: Vec<(Leftover, String)>,
    /// The folders whose lock cannot be told. They were left alone.
    pub unknown: Vec<Untold>,
    /// Why the temporary folder could not be listed, when it could not.
    pub unreadable: Option<String>,
}

/// Removes the webview folders CretLink left, trying for `patience` in all,
/// and says what it found, removed and had to keep.
#[must_use]
pub fn recover(places: &Places, patience: &mut Patience) -> Recovery {
    let mut recovery = Recovery::default();
    match look(&places.temp, None) {
        Ok(found) => {
            recovery.unknown = found.unknown;
            for (path, lock) in found.left {
                let leftover = Leftover {
                    path,
                    origin: Origin::Stale,
                };
                match remove_left(&leftover.path, lock, patience) {
                    Ok(true) => {
                        recovery.found.push(leftover.clone());
                        recovery.removed.push(leftover);
                    }
                    // A process was creating it: not a leftover.
                    Ok(false) => {}
                    Err(err) => {
                        recovery.found.push(leftover.clone());
                        recovery.kept.push((leftover, err.to_string()));
                    }
                }
            }
        }
        Err(err) => recovery.unreadable = Some(unreadable(&places.temp, &err)),
    }
    if let Some(leftover) = places.earlier_version() {
        recovery.found.push(leftover.clone());
        match remove_tree(&leftover.path, patience) {
            Ok(()) => recovery.removed.push(leftover),
            Err(err) => recovery.kept.push((leftover, err.to_string())),
        }
    }
    recovery
}

fn unreadable(temp: &Path, err: &io::Error) -> String {
    format!(
        "the temporary folder {} could not be listed ({})",
        temp.display(),
        err.kind()
    )
}

#[cfg(test)]
mod tests;
