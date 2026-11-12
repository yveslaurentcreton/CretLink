//! The folders CretLink keeps for itself on the customer computer
//! (ST-M006-020, REQ-S-024).
//!
//! The customer-side program keeps a staging area and a store of reboot
//! tickets. When it runs with administrator rights it acts with those rights
//! on what lies there: a cleanup removes the files a manifest names, and a
//! staged tool is run. A program of an account without rights must therefore
//! never have been able to put anything there or to open anything there.
//!
//! # Born private, in a folder only administrators can write
//!
//! Making an existing folder private afterwards is not enough. Windows checks
//! rights when an entry is opened: whoever opened a folder while it was open
//! to everyone keeps what that handle allows, whatever the permissions say
//! later. And nothing on a folder proves who made it: an entry that an
//! elevated program once created in a folder of an account keeps the
//! administrators as its owner, while that account may move it anywhere it
//! can write. So the elevated program's folders must be made by
//! administrators, private from the moment they exist:
//!
//! - **Where.** `CretLink Data` in `Program Files` on the drive of the Windows
//!   folder. Only administrators can create or move anything into
//!   `Program Files`, so whatever stands there was put there by one. The
//!   drive is asked from the system and nothing is read from the
//!   environment, which whoever starts the program can choose.
//!   `Program Files` itself must be a folder that Windows, the system or the
//!   administrators own; otherwise the program keeps nothing.
//! - **How.** [`OwnFolder::prepare`] creates each folder with its owner and
//!   its permissions in one step: owned by the administrators, with
//!   permissions of its own (not inherited) that give full control to the
//!   administrators and the system and nothing to anyone else. Everything
//!   made below inherits that. A folder that is already there is opened
//!   without following a link, and through that one handle its kind and its
//!   owner are read, its permissions are set if they are others, and the
//!   result is read back. Something else under one of those names is an
//!   error: the program then keeps nothing, and says so.
//!
//! A process that is elevated without being an administrator cannot make
//! anything private and keeps nothing.
//!
//! # Without administrator rights
//!
//! A program without administrator rights keeps its files in `CretLink` in
//! the local application data of its account (`%LOCALAPPDATA%`, as the
//! environment names it). No boundary is crossed there, so nothing is
//! checked. A program works only in the place of its own kind: an elevated
//! program never reads the place in a profile, and a program without rights
//! cannot read the other ([`administrators_folder`] lets it say so).
//!
//! # What an earlier release left
//!
//! Releases up to 0.24.0 kept these files in `CretLink` under the machine's
//! application data (`ProgramData`), where every account could create files
//! and folders. Nothing there is read any more. [`earlier_release_folder`]
//! names the folder so that a start can remove it whole, without following a
//! link ([`remove_whole`]), and a recovery can report it. An elevated program
//! asks the system where that folder is; a program without rights takes it
//! from its environment, as those releases did.
//!
//! Off Windows the agent is a development and test tool that is never elevated
//! on a customer computer: every place is the account's own.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// The folder of an elevated program's own files, in `Program Files`.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only an elevated Windows program has a place of this kind"
    )
)]
const PRIVATE_DATA_FOLDER: &str = "CretLink Data";

/// The folder of an unelevated program's own files, in the account's local
/// application data, and the folder that releases up to 0.24.0 used.
const DATA_FOLDER: &str = "CretLink";

/// The staging area inside the data folder.
const STAGING_FOLDER: &str = "staging";

/// The store of reboot tickets inside the data folder.
const TICKETS_FOLDER: &str = "reboot-resume";

/// The private folders this process has already prepared, so that an
/// operation that needs one does not open and check it again every time.
static PREPARED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// What stands at a name where CretLink makes a folder or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Nothing is there.
    Absent,
    /// An entry of the expected kind that is not a link.
    Ours,
    /// Something else: a link, or an entry of another kind. It is never
    /// read, entered or followed.
    Foreign,
}

impl Standing {
    /// What stands at `path`, where CretLink makes a folder.
    #[must_use]
    pub fn of_dir(path: &Path) -> Self {
        Self::of(path, true)
    }

    /// What stands at `path`, where CretLink makes a file.
    #[must_use]
    pub fn of_file(path: &Path) -> Self {
        Self::of(path, false)
    }

    fn of(path: &Path, dir: bool) -> Self {
        let facts = match std::fs::symlink_metadata(path) {
            Ok(facts) => facts,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::Absent,
            // What cannot be looked at cannot be told to be ours.
            Err(_) => return Self::Foreign,
        };
        let kind = facts.file_type();
        let expected = if dir { kind.is_dir() } else { kind.is_file() };
        if kind.is_symlink() || !expected {
            Self::Foreign
        } else {
            Self::Ours
        }
    }
}

/// A folder CretLink keeps for itself, and whether it must be private.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnFolder {
    /// The folder.
    path: PathBuf,
    /// The outermost folder that is CretLink's own, when the folder lies in
    /// one: [`Self::path`] itself or a folder above it.
    root: Option<PathBuf>,
    /// Whether the folders from the root down must be private: the folder
    /// belongs to an elevated program.
    private: bool,
}

impl OwnFolder {
    /// The staging area of this process: private when the process is
    /// elevated, the account's own otherwise.
    #[must_use]
    pub fn staging_for_this_process() -> Self {
        Self::of_this_process(STAGING_FOLDER)
    }

    /// The folder of the reboot tickets of this process: private when the
    /// process is elevated, the account's own otherwise.
    #[must_use]
    pub fn tickets_for_this_process() -> Self {
        Self::of_this_process(TICKETS_FOLDER)
    }

    fn of_this_process(name: &str) -> Self {
        let (root, private) = sys::data_root();
        Self {
            path: root.join(name),
            root: Some(root),
            private,
        }
    }

    /// A folder of the account that runs this process: a test sandbox, or the
    /// place of a program without administrator rights.
    #[must_use]
    pub fn same_account(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            root: None,
            private: false,
        }
    }

    /// A folder that must be private, together with every folder from
    /// `private_from` down to it. `private_from` is `path` or a folder above
    /// it; given anything else, `path` alone must be private.
    ///
    /// The folder that holds `private_from` must be one that only
    /// administrators can write: only then does "private" hold for what was
    /// there before (see the module documentation).
    #[must_use]
    pub fn administrators_only(private_from: impl Into<PathBuf>, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let private_from = private_from.into();
        let root = if path.starts_with(&private_from) {
            private_from
        } else {
            path.clone()
        };
        Self {
            path,
            root: Some(root),
            private: true,
        }
    }

    /// The folder.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the folder must be private: it belongs to an elevated program.
    #[must_use]
    pub fn is_private(&self) -> bool {
        self.private
    }

    /// Whether anything stands where the outermost folder of this one
    /// belongs. Where nothing does, CretLink never kept anything.
    #[must_use]
    pub fn has_a_data_folder(&self) -> bool {
        let outermost = self.root.as_deref().unwrap_or(&self.path);
        std::fs::symlink_metadata(outermost).is_ok()
    }

    /// What stands where this folder and CretLink's own folders above it
    /// belong, by kind alone: nothing ([`Standing::Absent`], also when only
    /// a folder above is there), folders ([`Standing::Ours`]), or a link or
    /// something else under one of the names ([`Standing::Foreign`]). It
    /// changes nothing.
    #[must_use]
    pub fn standing(&self) -> Standing {
        for dir in self.chain() {
            match Standing::of_dir(&dir) {
                Standing::Ours => {}
                other => return other,
            }
        }
        Standing::Ours
    }

    /// The folders that are CretLink's own on the way to this one, outermost
    /// first and this one last.
    fn chain(&self) -> Vec<PathBuf> {
        let Some(first) = &self.root else {
            return vec![self.path.clone()];
        };
        let mut chain = Vec::new();
        let mut current = Some(self.path.as_path());
        while let Some(dir) = current {
            chain.push(dir.to_path_buf());
            if dir == first {
                break;
            }
            current = dir.parent();
        }
        chain.reverse();
        chain
    }

    /// Makes the folder exist and, when it must be private, makes it and the
    /// folders above it private (see the module documentation).
    ///
    /// Blocking: it works on the file system.
    ///
    /// # Errors
    ///
    /// When this process is not an administrator; when something that is not
    /// a folder of the administrators stands under one of the names; or when
    /// a folder cannot be created, opened or made private, or does not show
    /// the permissions it was given. Nothing in the folder may be used then.
    pub fn prepare(&self) -> io::Result<()> {
        if !self.is_private() {
            return std::fs::create_dir_all(&self.path);
        }
        sys::require_administrator()?;
        for dir in self.chain() {
            sys::make_private(&dir)?;
        }
        PREPARED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.path.clone());
        Ok(())
    }

    /// Removes the folder and CretLink's own folders above it, as far as they
    /// are empty: innermost first, stopping at the first that holds
    /// something. For a recovery that left nothing, so that not even an empty
    /// folder stays behind. A link under one of the names is never followed.
    ///
    /// Blocking: it works on the file system.
    pub fn remove_if_empty(&self) {
        self.forget_prepared();
        for dir in self.chain().iter().rev() {
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
        }
    }

    /// [`Self::prepare`] for a private folder that this process has not
    /// prepared yet; nothing otherwise. Once a private folder is prepared,
    /// only an administrator can change it, so one check per process holds.
    ///
    /// # Errors
    ///
    /// See [`Self::prepare`].
    pub fn prepare_once(&self) -> io::Result<()> {
        if !self.is_private() {
            return Ok(());
        }
        let prepared = PREPARED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&self.path);
        if prepared {
            return Ok(());
        }
        self.prepare()
    }

    /// Forgets that this process prepared the folder, so that the next
    /// [`Self::prepare_once`] looks again: for a caller that found the folder
    /// gone.
    pub fn forget_prepared(&self) {
        PREPARED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.path);
    }
}

impl From<PathBuf> for OwnFolder {
    fn from(path: PathBuf) -> Self {
        Self::same_account(path)
    }
}

impl From<&PathBuf> for OwnFolder {
    fn from(path: &PathBuf) -> Self {
        Self::same_account(path.clone())
    }
}

impl From<&Path> for OwnFolder {
    fn from(path: &Path) -> Self {
        Self::same_account(path.to_path_buf())
    }
}

impl From<&OwnFolder> for OwnFolder {
    fn from(folder: &OwnFolder) -> Self {
        folder.clone()
    }
}

/// The folder in which a program with administrator rights keeps its files,
/// whether or not this process is one: `CretLink Data` in `Program Files`.
/// `None` off Windows, and when `Program Files` is not one of the system's own
/// folders.
///
/// For a program without those rights, which cannot read the folder, to say
/// that there is something it cannot see.
#[must_use]
pub fn administrators_folder() -> Option<PathBuf> {
    sys::administrators_folder()
}

/// The folder in which releases up to 0.24.0 kept the staging area and the
/// reboot tickets: `CretLink` under the machine's application data, where
/// every account could create files and folders. `None` off Windows, and for
/// an elevated program when the application data folder is not one of the
/// system's own.
///
/// Nothing in it is read. It is only removed whole ([`remove_whole`]) or
/// reported.
#[must_use]
pub fn earlier_release_folder() -> Option<PathBuf> {
    sys::earlier_release_folder()
}

/// Whether a program of an earlier release seems to be running a session out
/// of `folder`, its data folder: it then holds the lock of its staging area.
///
/// The lock is looked at only where a file stands under its name in folders:
/// with a link in the place of the data folder, of its staging area or of the
/// lock, nobody is taken to hold it. The lock is then opened for reading as
/// what stands under its name, tried and given back; nothing is read from it
/// and it is never waited for.
#[must_use]
pub fn earlier_release_in_use(folder: &Path) -> bool {
    let staging = folder.join(STAGING_FOLDER);
    let lock = staging.join(".in-use.lock");
    Standing::of_dir(folder) == Standing::Ours
        && Standing::of_dir(&staging) == Standing::Ours
        && Standing::of_file(&lock) == Standing::Ours
        && sys::locked_by_another(&lock)
}

/// Removes what stands at `path` whole, without reading it and without
/// following a link: a link is removed as a link, a folder with everything in
/// it. For what CretLink no longer uses or did not make, where nothing in it
/// may be trusted.
///
/// Blocking: it works on the file system.
///
/// # Errors
///
/// When the entry, or something in it, cannot be removed.
pub fn remove_whole(path: &Path) -> io::Result<()> {
    let facts = std::fs::symlink_metadata(path)?;
    let kind = facts.file_type();
    if kind.is_symlink() {
        // On Windows a link to a folder is removed as a folder.
        std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
    } else if kind.is_dir() {
        // The standard library removes a tree through handles and never
        // follows a link in it.
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Whether the owner and the permissions of a folder, in the text form of a
/// security descriptor, are the private ones: owned by the administrators or
/// the system; a list of its own that refuses what the folder above would
/// hand down; and full control, also for what is made in the folder later,
/// for exactly those two.
///
/// The text is what the system writes for a descriptor read back from a
/// folder, for example `O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only the Windows adapter reads permissions; the rule is tested everywhere"
    )
)]
fn is_private_text(text: &str) -> bool {
    /// The two spellings the system has for full control over a file.
    const FULL_CONTROL: [&str; 2] = ["FA", "0x1f01ff"];
    /// Who has full control over a private folder.
    const WHO: [&str; 2] = ["SY", "BA"];

    let Some(rest) = text.strip_prefix("O:") else {
        return false;
    };
    let Some((owner_and_group, access)) = rest.split_once("D:") else {
        return false;
    };
    // The owner is followed by the group when the system writes one.
    let owner = owner_and_group.split("G:").next().unwrap_or_default();
    if !WHO.contains(&owner) {
        return false;
    }
    // A list for auditing, if the system writes one, is not part of this rule.
    let access = access.split("S:").next().unwrap_or_default();
    let (flags, entries) = match access.find('(') {
        Some(at) => access.split_at(at),
        None => (access, ""),
    };
    // `P`: protected, so nothing is inherited from the folder above.
    if !flags.contains('P') {
        return false;
    }
    let mut seen = [false; WHO.len()];
    for entry in entries.split_inclusive(')') {
        let Some(fields) = entry
            .strip_prefix('(')
            .and_then(|entry| entry.strip_suffix(')'))
        else {
            return false;
        };
        let fields: Vec<&str> = fields.split(';').collect();
        let [kind, handed_down, rights, object, inherited_object, who] = fields[..] else {
            return false;
        };
        let Some(index) = WHO.iter().position(|known| *known == who) else {
            return false;
        };
        let full = FULL_CONTROL
            .iter()
            .any(|spelling| rights.eq_ignore_ascii_case(spelling));
        // `OICI`: handed down to the files and the folders made in it.
        if kind != "A"
            || handed_down != "OICI"
            || !full
            || !object.is_empty()
            || !inherited_object.is_empty()
            || seen[index]
        {
            return false;
        }
        seen[index] = true;
    }
    seen.iter().all(|seen| *seen)
}

#[cfg(not(windows))]
mod sys {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::{DATA_FOLDER, Standing};

    /// The folder for CretLink's own files: off Windows always a place of the
    /// account, where the earlier releases kept it.
    pub(super) fn data_root() -> (PathBuf, bool) {
        let root = match std::env::var_os("ProgramData") {
            Some(program_data) => PathBuf::from(program_data).join(DATA_FOLDER),
            None => std::env::temp_dir().join("cretlink"),
        };
        (root, false)
    }

    /// Off Windows no program runs with administrator rights.
    pub(super) fn administrators_folder() -> Option<PathBuf> {
        None
    }

    /// Off Windows the earlier releases kept their files where this one does.
    pub(super) fn earlier_release_folder() -> Option<PathBuf> {
        None
    }

    /// Off Windows there is no earlier release to look for.
    pub(super) fn locked_by_another(_lock: &Path) -> bool {
        false
    }

    /// Off Windows there is no administrator to be: see the module
    /// documentation.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the Windows adapter of the same name can fail"
    )]
    pub(super) fn require_administrator() -> io::Result<()> {
        Ok(())
    }

    /// Makes `dir` exist. Off Windows nothing is made private; something that
    /// is not a folder under the name is an error all the same.
    pub(super) fn make_private(dir: &Path) -> io::Result<()> {
        match std::fs::create_dir(dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        match Standing::of_dir(dir) {
            Standing::Ours => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "something that is not a folder stands in its place",
            )),
        }
    }
}

#[cfg(windows)]
mod sys {
    #![allow(
        unsafe_code,
        reason = "Localized Win32 calls for the folders CretLink keeps for \
                  itself (ST-M006-020): whether this process is elevated \
                  and an administrator, and \
                  the owner and the permissions of a folder, set when it is \
                  created and read and set through its handle. ADR-0001 \
                  permits reviewed, localized unsafe in one platform adapter."
    )]

    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_OWNER, ERROR_SUCCESS, HANDLE, HLOCAL,
        LocalFree,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetTokenInformation,
        IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, SetKernelObjectSecurity, TOKEN_ELEVATION,
        TOKEN_QUERY, TokenElevation, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateDirectoryW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
        WRITE_DAC,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};

    use super::{DATA_FOLDER, PRIVATE_DATA_FOLDER, is_private_text};

    /// The private permissions of a folder with their owner, in the text form
    /// of a security descriptor: owned by the administrators (`O:BA`); a list
    /// that is protected from what the folder above would hand down (`P`);
    /// full control for the system and for the administrators, for the folder
    /// and everything made in it later (`OICI`).
    const PRIVATE_WITH_OWNER: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

    /// The same permissions without an owner, for a folder that has one.
    const PRIVATE: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

    /// The service that owns the files of Windows itself, as the system
    /// writes its identifier. It owns `Program Files`.
    const TRUSTED_INSTALLER: &str =
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

    /// The folder on the drive of the Windows folder that only administrators
    /// can write, and in which an elevated program keeps its own files.
    const ADMINISTRATORS_FOLDER: &str = "Program Files";

    /// The folder for application data on the drive of the Windows folder,
    /// which every account can write.
    const MACHINE_DATA: &str = "ProgramData";

    /// A path under which nothing can be made or found: a name with a NUL is
    /// refused by every file call. Used when the place for an elevated
    /// program's files cannot be told, so that it then keeps nothing rather
    /// than falling back to a place someone else could choose.
    const NOWHERE: &str = "\0cretlink-has-no-folder-of-its-own";

    // File attribute bits, as `std::os::windows::fs::MetadataExt` reports them.
    const ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    /// The folder for CretLink's own files, and whether it must be private:
    /// `CretLink Data` in `Program Files` for an elevated process, `CretLink`
    /// in the account's local application data otherwise.
    pub(super) fn data_root() -> (PathBuf, bool) {
        if elevated() {
            let root = administrators_folder().unwrap_or_else(|| PathBuf::from(NOWHERE));
            (root, true)
        } else {
            // A program without rights acts for its own account only, so it
            // may take the place from its environment like any other program.
            let root = std::env::var_os("LOCALAPPDATA")
                .filter(|folder| !folder.is_empty())
                .map_or_else(
                    || std::env::temp_dir().join("cretlink"),
                    |folder| PathBuf::from(folder).join(DATA_FOLDER),
                );
            (root, false)
        }
    }

    /// Where an elevated program keeps its files: `CretLink Data` in
    /// `Program Files`.
    pub(super) fn administrators_folder() -> Option<PathBuf> {
        system_folder(ADMINISTRATORS_FOLDER).map(|folder| folder.join(PRIVATE_DATA_FOLDER))
    }

    /// Where releases up to 0.24.0 kept their files: `CretLink` in the
    /// machine's application data.
    ///
    /// An elevated program asks the system where that is. A program without
    /// rights takes it from its environment, as those releases did and as it
    /// takes its own place: it acts for its own account only.
    pub(super) fn earlier_release_folder() -> Option<PathBuf> {
        let machine_data = if elevated() {
            system_folder(MACHINE_DATA)
        } else {
            std::env::var_os("ProgramData")
                .filter(|folder| !folder.is_empty())
                .map(PathBuf::from)
                .or_else(|| system_folder(MACHINE_DATA))
        };
        machine_data.map(|folder| folder.join(DATA_FOLDER))
    }

    /// The folder `name` on the drive of the Windows folder, which the system
    /// names. `None` when it is not a folder that Windows, the system or the
    /// administrators own: then it is not the folder this program expects,
    /// and nothing below it is used.
    fn system_folder(name: &str) -> Option<PathBuf> {
        let windows = crate::system::windows_folder()?;
        let drive = windows.ancestors().last()?;
        let folder = drive.join(name);
        let opened = open(&folder, READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0).ok()?;
        let owner = Owner::of(&opened).ok()?;
        let trusted = owner.is_trusted() || owner.text().as_deref() == Some(TRUSTED_INSTALLER);
        (is_plain_folder(&opened).ok()? && trusted).then_some(folder)
    }

    /// Whether another program holds a lock on the file at `lock`. The file
    /// is opened as what stands under its name, for reading only, and the
    /// lock is tried and given back at once.
    pub(super) fn locked_by_another(lock: &Path) -> bool {
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(lock);
        match opened {
            Ok(file) => match file.try_lock() {
                Ok(()) => {
                    let _ = file.unlock();
                    false
                }
                Err(std::fs::TryLockError::WouldBlock) => true,
                Err(std::fs::TryLockError::Error(_)) => false,
            },
            Err(_) => false,
        }
    }

    /// Whether this process is elevated. A token that cannot be read counts
    /// as elevated: the program then keeps to the private place, where it can
    /// do nothing if it has no rights, instead of acting with rights it may
    /// have on a place of the account.
    pub(super) fn elevated() -> bool {
        let mut token = HANDLE::default();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is always
        // valid and is not closed; `token` is a valid place for the result.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
            return true;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        // SAFETY: `token` was just opened for querying; the buffer is an
        // initialized `TOKEN_ELEVATION`, the structure of this class, passed
        // with its size, and `returned` is a valid place for the length.
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                Some(std::ptr::from_mut(&mut elevation).cast::<c_void>()),
                size_of_u32::<TOKEN_ELEVATION>(),
                &mut returned,
            )
        };
        // SAFETY: `token` is the handle opened above and is closed once.
        let _ = unsafe { CloseHandle(token) };
        match read {
            Ok(()) => elevation.TokenIsElevated != 0,
            Err(_) => true,
        }
    }

    /// Fails unless this process acts as a member of the administrators: only
    /// such a process can make a folder in `Program Files` and own it as one
    /// of them. An elevated process of another kind keeps nothing.
    pub(super) fn require_administrator() -> io::Result<()> {
        // Words, not bytes, so that the SID in them is aligned.
        let mut sid = [0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
        let mut len = SECURITY_MAX_SID_SIZE;
        let administrators = PSID(sid.as_mut_ptr().cast::<c_void>());
        // SAFETY: the buffer is writable and at least as large as the largest
        // SID, whose size is passed in `len`.
        unsafe {
            CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                None,
                Some(administrators),
                &mut len,
            )
        }
        .map_err(|error| os_error(&error))?;
        let mut member = BOOL::default();
        // SAFETY: no token is passed, so the token this thread acts with is
        // asked; the SID lives in `sid` until after the call, and `member` is
        // a valid place for the answer.
        unsafe { CheckTokenMembership(None, administrators, &mut member) }
            .map_err(|error| os_error(&error))?;
        if member.as_bool() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "this program does not run as an administrator",
            ))
        }
    }

    /// Makes `dir` a private folder: creates it with its owner and the
    /// private permissions in one step when nothing stands there; otherwise
    /// opens what stands there without following a link and, through that one
    /// handle, reads its kind and its owner, sets the private permissions if
    /// it has others, and reads them back.
    ///
    /// The folder above `dir` is one only administrators can write, so
    /// whatever stands at `dir` was put there by one of them. Something that
    /// is not a folder of theirs is an error all the same, and nothing below
    /// it is used.
    pub(super) fn make_private(dir: &Path) -> io::Result<()> {
        create(dir)?;
        let folder = open(dir, READ_CONTROL.0 | WRITE_DAC.0 | FILE_READ_ATTRIBUTES.0)?;
        if !is_plain_folder(&folder)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "something that is not a folder stands in its place",
            ));
        }
        if !Owner::of(&folder)?.is_trusted() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the folder does not belong to the administrators",
            ));
        }
        if is_private_text(&permissions_text(&folder)?) {
            return Ok(());
        }
        let private = Descriptor::from_text(PRIVATE)?;
        // SAFETY: the handle is open with the right to change permissions for
        // as long as `folder` lives, and `private` owns a valid descriptor.
        unsafe {
            SetKernelObjectSecurity(
                HANDLE(folder.as_raw_handle()),
                DACL_SECURITY_INFORMATION,
                private.0,
            )
        }
        .map_err(|error| os_error(&error))?;
        if is_private_text(&permissions_text(&folder)?) {
            Ok(())
        } else {
            Err(io::Error::other(
                "the folder does not show the permissions it was given",
            ))
        }
    }

    /// Creates `dir` with its owner and the private permissions. A name that
    /// is taken is not an error: what stands there is looked at by the
    /// caller.
    fn create(dir: &Path) -> io::Result<()> {
        let path = HSTRING::from(dir.as_os_str());
        for text in [PRIVATE_WITH_OWNER, PRIVATE] {
            let descriptor = Descriptor::from_text(text)?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of_u32::<SECURITY_ATTRIBUTES>(),
                lpSecurityDescriptor: descriptor.0.0,
                bInheritHandle: false.into(),
            };
            // SAFETY: `path` is a NUL-terminated wide string and `attributes`
            // points to a valid descriptor, both for the duration of the call.
            match unsafe { CreateDirectoryW(PCWSTR(path.as_ptr()), Some(&attributes)) } {
                Ok(()) => return Ok(()),
                Err(error) if error.code() == ERROR_ALREADY_EXISTS.to_hresult() => return Ok(()),
                // This account may not name the administrators as the owner:
                // the folder is made without naming one, and the owner it
                // then has is judged like that of any folder found.
                Err(error) if error.code() == ERROR_INVALID_OWNER.to_hresult() => {}
                Err(error) => return Err(os_error(&error)),
            }
        }
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }

    /// The owner and the permissions of the entry at `path`, for the tests.
    #[cfg(test)]
    pub(super) fn permissions_of(path: &Path) -> io::Result<String> {
        permissions_text(&open(path, READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0)?)
    }

    /// Opens the entry at `path` itself, never what a link there points at,
    /// leaving every other use of it by others allowed.
    fn open(path: &Path, access: u32) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(access)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
            // `BACKUP_SEMANTICS` lets a folder be opened; `OPEN_REPARSE_POINT`
            // opens a link instead of following it.
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
    }

    /// Whether an opened entry is a folder, and no link or other reparse
    /// point.
    fn is_plain_folder(entry: &File) -> io::Result<bool> {
        let attributes = entry.metadata()?.file_attributes();
        Ok(attributes & ATTRIBUTE_DIRECTORY != 0 && attributes & ATTRIBUTE_REPARSE_POINT == 0)
    }

    /// The owner of an opened entry, with the descriptor that holds it.
    struct Owner {
        sid: PSID,
        _descriptor: Descriptor,
    }

    impl Owner {
        fn of(entry: &File) -> io::Result<Self> {
            let mut sid = PSID::default();
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            // SAFETY: the handle is open with the right to read permissions
            // for as long as `entry` is borrowed; `sid` and `descriptor` are
            // valid places for the results.
            let status = unsafe {
                GetSecurityInfo(
                    HANDLE(entry.as_raw_handle()),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION,
                    Some(&mut sid),
                    None,
                    None,
                    None,
                    Some(&mut descriptor),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status.0.cast_signed()));
            }
            // `sid` points into the descriptor, which this value keeps alive.
            Ok(Self {
                sid,
                _descriptor: Descriptor(descriptor),
            })
        }

        /// Whether the owner is the administrators or the system.
        fn is_trusted(&self) -> bool {
            if self.sid.is_invalid() {
                return false;
            }
            // SAFETY: `sid` points to a valid SID inside the descriptor that
            // `self` keeps alive.
            unsafe {
                IsWellKnownSid(self.sid, WinBuiltinAdministratorsSid).as_bool()
                    || IsWellKnownSid(self.sid, WinLocalSystemSid).as_bool()
            }
        }

        /// The owner's identifier as the system writes it.
        fn text(&self) -> Option<String> {
            if self.sid.is_invalid() {
                return None;
            }
            let mut text = PWSTR::null();
            // SAFETY: `sid` points to a valid SID inside the descriptor that
            // `self` keeps alive, and `text` is a valid place for the string
            // the system allocates.
            unsafe { ConvertSidToStringSidW(self.sid, &mut text) }.ok()?;
            // SAFETY: on success `text` is a NUL-terminated wide string that
            // stays valid until it is freed below.
            let read = String::from_utf16_lossy(unsafe { text.as_wide() });
            // SAFETY: the string was allocated by the system for the caller
            // to free with `LocalFree`, and is not used afterwards.
            let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast::<c_void>()))) };
            Some(read)
        }
    }

    /// The owner and the permissions of an opened entry, in the text form of
    /// a security descriptor.
    fn permissions_text(entry: &File) -> io::Result<String> {
        let wanted = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the handle is open with the right to read permissions for
        // as long as `entry` is borrowed; `descriptor` is a valid place for
        // the result.
        let status = unsafe {
            GetSecurityInfo(
                HANDLE(entry.as_raw_handle()),
                SE_FILE_OBJECT,
                wanted,
                None,
                None,
                None,
                None,
                Some(&mut descriptor),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status.0.cast_signed()));
        }
        let descriptor = Descriptor(descriptor);
        let mut text = PWSTR::null();
        // SAFETY: `descriptor` owns a valid descriptor, and `text` is a valid
        // place for the string the system allocates.
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                wanted,
                &mut text,
                None,
            )
        }
        .map_err(|error| os_error(&error))?;
        // SAFETY: on success `text` is a NUL-terminated wide string that
        // stays valid until it is freed below.
        let read = String::from_utf16_lossy(unsafe { text.as_wide() });
        // SAFETY: the string was allocated by the system for the caller to
        // free with `LocalFree`, and is not used afterwards.
        let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast::<c_void>()))) };
        Ok(read)
    }

    /// A security descriptor that the system allocated, freed when dropped.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Descriptor {
        /// The descriptor that `text` describes.
        fn from_text(text: &str) -> io::Result<Self> {
            let text = HSTRING::from(text);
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            // SAFETY: `text` is a NUL-terminated wide string for the duration
            // of the call, and `descriptor` is a valid place for the result.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(text.as_ptr()),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
            }
            .map_err(|error| os_error(&error))?;
            Ok(Self(descriptor))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: the descriptor was allocated by the system for the
                // caller to free with `LocalFree`, and is freed exactly once.
                let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
            }
        }
    }

    /// The size of `T` as the `u32` the Win32 calls take.
    fn size_of_u32<T>() -> u32 {
        u32::try_from(size_of::<T>()).unwrap_or(0)
    }

    /// The I/O error for a failed Win32 call, carrying the Windows error code
    /// so that it is classified like any other. The `windows` crate reports
    /// such a failure as an `HRESULT` that holds the code in its low half.
    fn os_error(error: &windows::core::Error) -> io::Error {
        const WIN32_FACILITY: u32 = 0x8007_0000;
        let hresult = error.code().0.cast_unsigned();
        if hresult & 0xFFFF_0000 == WIN32_FACILITY {
            io::Error::from_raw_os_error((hresult & 0xFFFF).cast_signed())
        } else {
            io::Error::other("a call to the system failed")
        }
    }
}

/// Whether the tests that need a private folder can run on Windows: they
/// name the administrators as an owner, which only an administrator may. In
/// continuous verification they must run, so nothing is skipped there.
#[cfg(all(test, windows))]
pub(crate) fn tests_can_name_the_administrators() -> bool {
    if sys::require_administrator().is_ok() || std::env::var_os("CI").is_some() {
        return true;
    }
    eprintln!("skipped: this process is not an administrator");
    false
}

/// Whether the folder at `path` has the private permissions, for tests
/// outside this module.
#[cfg(all(test, windows))]
pub(crate) fn tests_is_private(path: &Path) -> bool {
    sys::permissions_of(path).is_ok_and(|text| is_private_text(&text))
}

/// Whether the permissions of the entry at `path` give rights to nobody but
/// the administrators and the system, for tests outside this module: what is
/// made in a private folder is closed like that by what the folder hands down.
#[cfg(all(test, windows))]
pub(crate) fn tests_closed_to_others(path: &Path) -> bool {
    let Ok(text) = sys::permissions_of(path) else {
        return false;
    };
    let Some((_, access)) = text.split_once("D:") else {
        return false;
    };
    let named: Vec<&str> = access
        .split(')')
        .filter_map(|entry| entry.rsplit(';').next())
        .filter(|who| !who.is_empty() && !who.contains('('))
        .collect();
    !named.is_empty() && named.iter().all(|who| *who == "SY" || *who == "BA")
}

#[cfg(test)]
mod tests;
