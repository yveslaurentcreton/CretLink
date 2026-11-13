//! A folder that is opened once, verified and then held (ST-M010-001,
//! ADR-0012).
//!
//! The agent that runs this is elevated, and the folders it is asked to work
//! in may belong to an account that is not. A program of that account can
//! turn a path against the rest of the computer: make a folder on the way a
//! junction or a symbolic link, or swap a folder for a link between two uses
//! of its path, so that "this folder" names another place by the time the
//! path is resolved again.
//!
//! [`Folder::open`] is the one answer to that, for every operation that
//! changes something in a folder it was given by its path:
//!
//! - **The path is resolved once.** The folder is opened, and everything
//!   after that happens through the opened folder: entries in it are opened
//!   by their one name **relative to that handle**, and renamed and removed
//!   through their own handles. No path is resolved a second time.
//! - **The opened folder must lie where the path says.** The real location of
//!   what was opened is read from the handle and compared with the path as it
//!   was asked. If they differ, a link was followed on the way, and the
//!   folder is refused: the technician asked for one place and would get
//!   another.
//! - **A folder has an identity.** The volume and the file identifier of the
//!   opened folder stay the same for as long as it exists, whatever happens
//!   to its name. An operation of several requests gives the identity back
//!   with each request and so keeps to the folder it began with.
//!
//! # Windows
//!
//! The last part of the path is never followed: a junction or a symbolic
//! link in that place is opened as the link it is, and the caller decides
//! what a link there means. The real location is the final path of the
//! handle in its drive-letter form. A path on a drive made with `subst`, on a
//! network drive, or with a part that Win32 would rewrite (a name that ends
//! in a dot or a space) is not in a form whose location can be compared, and
//! is refused like a path through a link.
//!
//! A name inside the folder is opened with `NtCreateFile` and the folder's
//! handle as its root directory. The file system looks the name up in the
//! opened directory itself, so neither a path, nor a junction, nor a link in
//! the object manager's namespace takes part. The rename that publishes a
//! file goes through `NtSetInformationFile` with a bare name and no root
//! directory: at that level this is a rename inside the directory the file
//! lies in. (The Win32 wrapper `SetFileInformationByHandle` is not used for
//! it: it reads a bare name as a path relative to the current directory of
//! the process.)
//!
//! # Elsewhere
//!
//! Off Windows the same interface works on paths and verifies nothing but the
//! kind and the identity of the folder: a non-Windows agent is a development
//! and test tool that is never elevated on a customer computer.

use std::path::Path;

/// What an entry is, read without following a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A folder that holds its own content.
    Dir,
    /// An entry that stands for another place: a symbolic link, a junction,
    /// a volume mount point.
    Link,
    /// Anything else: a file, or a device, pipe or socket.
    File,
}

/// How a folder is held while it is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hold {
    /// For work inside the folder through its handle. Others keep every use
    /// of the folder: nothing the work does depends on its name.
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "only the Windows file store works inside an opened folder"
        )
    )]
    Shared,
    /// For removing the folder. For as long as it is held nobody else renames
    /// or removes it. The right to remove it is asked for with it; a folder
    /// that does not grant that right, or that another program has open in a
    /// way that excludes it, is held with the right to list it instead and is
    /// removed at the end through a second open. A folder that can be held in
    /// neither way is not taken up.
    ForRemoval,
}

/// Why a folder was not taken up.
#[derive(Debug)]
pub(super) enum Refusal {
    /// The operating system refused.
    Io(std::io::Error),
    /// What the path leads to does not lie where the path says: a link was
    /// followed on the way, or the path is not in a form whose real location
    /// can be compared.
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "only the Windows agent verifies where an opened folder lies"
        )
    )]
    Elsewhere,
}

impl From<std::io::Error> for Refusal {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// The fixed, path-free words for [`Refusal::Elsewhere`].
pub(super) const LEADS_ELSEWHERE: &str =
    "the path leads through a link or is not the folder's real location";

pub(super) use sys::Folder;
#[cfg(windows)]
pub(super) use sys::{
    Disposition, delete, kind_of, link_count, open_at, open_entry, open_for_removal, place_of,
    rename_in_folder,
};

/// The text of a path for a result or a report. On Windows a path built from
/// a verified folder is in the verbatim form (`\\?\C:\...`); the prefix is an
/// instruction to Win32, not a part of the location, and is left out.
pub(super) fn plain(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    if let Some(rest) = text.strip_prefix(VERBATIM_TEXT)
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return rest.to_owned();
    }
    text.into_owned()
}

/// The prefix of a verbatim path, which Win32 passes on without rewriting it.
#[cfg(windows)]
const VERBATIM_TEXT: &str = r"\\?\";

/// The same prefix in UTF-16.
#[cfg(any(windows, test))]
const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// The path separator of Windows in UTF-16.
#[cfg(any(windows, test))]
const SEPARATOR: u16 = b'\\' as u16;

/// Brings a path as it was asked into the form in which a real location is
/// written, so that the two can be compared: `X:\` followed by the parts of
/// the path, one separator between them and none at the end.
///
/// `/` counts as a separator and empty parts are dropped, as Win32 does.
/// `None` when the path is not rooted on a drive letter, or when it holds a
/// part that Win32 would rewrite before it opens anything: a part that ends
/// in a dot or a space (`.` and `..` among them). Such a path does not name
/// one location by its text, so there is nothing to compare.
///
/// A path that is already verbatim (`\\?\X:\...`) is taken as it is, without
/// its prefix: Win32 rewrites nothing in it.
#[cfg(any(windows, test))]
fn clean(asked: &[u16]) -> Option<Vec<u16>> {
    const SLASH: u16 = b'/' as u16;
    const COLON: u16 = b':' as u16;
    const DOT: u16 = b'.' as u16;
    const SPACE: u16 = b' ' as u16;

    let (verbatim, rest) = match asked.strip_prefix(&VERBATIM) {
        Some(rest) => (true, rest),
        None => (false, asked),
    };
    let is_separator = |unit: u16| unit == SEPARATOR || (!verbatim && unit == SLASH);
    let [drive, colon, root, parts @ ..] = rest else {
        return None;
    };
    let is_letter = u8::try_from(*drive).is_ok_and(|letter| letter.is_ascii_alphabetic());
    if !is_letter || *colon != COLON || !is_separator(*root) {
        return None;
    }
    let mut cleaned = vec![*drive, COLON, SEPARATOR];
    if parts.is_empty() {
        // The root of the drive.
        return Some(cleaned);
    }
    for part in parts.split(|unit| is_separator(*unit)) {
        let Some(last) = part.last() else {
            if verbatim {
                // An empty part is not dropped in a verbatim path.
                return None;
            }
            continue;
        };
        if !verbatim && (*last == DOT || *last == SPACE) {
            return None;
        }
        if cleaned.len() > 3 {
            cleaned.push(SEPARATOR);
        }
        cleaned.extend_from_slice(part);
    }
    Some(cleaned)
}

/// The UTF-16 form of one name inside a folder, or `None` when `name` is not
/// one plain name: it is empty, `.` or `..`, or it holds a separator, a colon
/// (which would name a stream of a file) or a NUL.
#[cfg(any(windows, test))]
fn single_name(name: &str) -> Option<Vec<u16>> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':', '\0']) {
        return None;
    }
    Some(name.encode_utf16().collect())
}

#[cfg(not(windows))]
mod sys {
    use std::path::{Path, PathBuf};

    use super::{Hold, Kind, Refusal};

    /// A folder taken up by its path. Off Windows it holds nothing: see the
    /// module documentation.
    #[derive(Debug)]
    pub(in super::super) struct Folder {
        path: PathBuf,
        kind: Kind,
        identity: Option<Vec<u8>>,
    }

    impl Folder {
        /// Takes up what `path` names, without following a link in the last
        /// part of the path, and reads what it is.
        pub(in super::super) fn open(path: &Path, _hold: Hold) -> Result<Self, Refusal> {
            let facts = std::fs::symlink_metadata(path)?;
            let kind = facts.file_type();
            let kind = if kind.is_symlink() {
                Kind::Link
            } else if kind.is_dir() {
                Kind::Dir
            } else {
                Kind::File
            };
            Ok(Self {
                path: path.to_path_buf(),
                kind,
                identity: identity(&facts),
            })
        }

        /// What was taken up.
        pub(in super::super) fn kind(&self) -> Kind {
            self.kind
        }

        /// The path from which the paths of the entries in the folder are
        /// built.
        pub(in super::super) fn path(&self) -> &Path {
            &self.path
        }

        /// Whether this is a whole drive rather than a folder on one: `/` has
        /// no folder above it.
        pub(in super::super) fn is_drive(&self) -> bool {
            self.path.parent().is_none()
        }

        /// What tells this folder apart from every other for as long as it
        /// exists, or `None` on a system that offers nothing of the kind.
        pub(in super::super) fn identity(&self) -> Option<Vec<u8>> {
            self.identity.clone()
        }

        /// Removes the folder, which must be empty, or the link or file that
        /// was taken up in its place. Neither call follows a link in the last
        /// part of the name: `rmdir` refuses a link, and `unlink` removes it.
        pub(in super::super) fn remove(self) -> std::io::Result<()> {
            match self.kind {
                Kind::Dir => std::fs::remove_dir(&self.path),
                Kind::Link | Kind::File => std::fs::remove_file(&self.path),
            }
        }
    }

    /// The device and the inode of an entry: together they name one entry for
    /// as long as it exists.
    #[cfg(unix)]
    fn identity(facts: &std::fs::Metadata) -> Option<Vec<u8>> {
        use std::os::unix::fs::MetadataExt;

        /// Marks an identity made of a device and an inode.
        const DEVICE_AND_INODE: u8 = 3;
        let mut identity = Vec::with_capacity(17);
        identity.push(DEVICE_AND_INODE);
        identity.extend_from_slice(&facts.dev().to_le_bytes());
        identity.extend_from_slice(&facts.ino().to_le_bytes());
        Some(identity)
    }

    /// A system this crate knows no identity of an entry for.
    #[cfg(not(unix))]
    fn identity(_facts: &std::fs::Metadata) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(windows)]
mod sys {
    #![allow(
        unsafe_code,
        reason = "Localized Win32 and native calls on file handles behind the \
                  verified folder (ST-M010-001): where an opened entry really \
                  lies and what identifies it, an open relative to an opened \
                  folder, and a rename and a removal through a handle. ADR-0001 \
                  and ADR-0006 permit reviewed, localized unsafe in one \
                  platform adapter."
    )]

    use std::ffi::{OsString, c_void};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};

    use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_INFORMATION_CLASS, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
        FILE_RENAME_INFORMATION, FILE_RENAME_INFORMATION_0, FILE_RENAME_POSIX_SEMANTICS,
        FILE_RENAME_REPLACE_IF_EXISTS, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
        FileRenameInformationEx, NTCREATEFILE_CREATE_DISPOSITION, NTCREATEFILE_CREATE_OPTIONS,
        NtCreateFile, NtSetInformationFile,
    };
    use windows::Win32::Foundation::{
        HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_FILE_IS_A_DIRECTORY,
        UNICODE_STRING,
    };
    use windows::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_NORMAL,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
        FILE_DISPOSITION_INFO_EX_FLAGS, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_WRITE_ATTRIBUTES, FileDispositionInfo, FileDispositionInfoEx, FileIdInfo,
        GETFINALPATHNAMEBYHANDLE_FLAGS, GetFileInformationByHandle, GetFileInformationByHandleEx,
        GetFinalPathNameByHandleW, GetLongPathNameW, SYNCHRONIZE, SetFileInformationByHandle,
        VOLUME_NAME_DOS, VOLUME_NAME_NT,
    };
    use windows::Win32::System::IO::IO_STATUS_BLOCK;
    use windows::core::{PCWSTR, PWSTR};

    use super::{Hold, Kind, Refusal, VERBATIM, clean, single_name};

    /// The longest path the calls below read, in UTF-16 units. A path on
    /// Windows has at most 32767 of them; a prefix adds a few.
    const MAX_PATH_UNITS: usize = 33_000;

    /// The length below which a path is given to Win32 as it is. A longer one
    /// needs the verbatim prefix to be accepted at all.
    const PLAIN_PATH_UNITS: usize = 248;

    /// Every use of an entry by others stays allowed.
    const SHARE_ALL: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;

    /// Others may read and change what is in a folder, but not rename or
    /// remove the folder itself: both need the right to delete, which this
    /// share mode keeps from every other handle. Windows applies a share mode
    /// only for a handle that itself may read, write or delete; on a handle
    /// that may only read attributes it keeps nobody from anything, so a
    /// folder is never held with such a handle.
    const SHARE_HELD: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0;

    /// What a folder is held with when the right to remove it is not to be
    /// had: the right to list it, which the removal of its content needs
    /// anyway, and with which Windows applies the share mode.
    const TO_HOLD: u32 = FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0;

    // Windows error codes that say the extended form of a removal or a rename
    // is not offered here: by an older Windows, or by a file system such as
    // FAT.
    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    const ERROR_INVALID_PARAMETER: i32 = 87;

    // Windows error codes that say another handle excludes this use.
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;

    // The plain form of a rename has one setting, a byte at the start of
    // its structure: whether a file of the new name is replaced. It shares
    // its place with the flags of the extended form.
    const PLAIN_KEEP: u32 = 0;
    const PLAIN_REPLACE: u32 = 1;

    /// Marks an identity made of the volume serial number and the 128-bit
    /// file identifier.
    const VOLUME_AND_FILE_ID: u8 = 1;

    /// Marks an identity made of the short volume serial number and the
    /// 64-bit file index, on a file system without the longer identifier.
    const VOLUME_AND_FILE_INDEX: u8 = 2;

    // The rename structure is written into a buffer of `u64`s.
    const _: () = assert!(align_of::<FILE_RENAME_INFORMATION>() <= align_of::<u64>());

    /// A folder that was opened by its path, verified to lie where that path
    /// says, and is held open. See the module documentation.
    #[derive(Debug)]
    pub(in super::super) struct Folder {
        /// The opened folder — or the link or the file in its place.
        handle: File,
        /// What was opened.
        kind: Kind,
        /// Where it really lies, in the verbatim form.
        path: PathBuf,
        /// Whether that location is the root of a drive.
        drive: bool,
        /// Whether the handle carries the right to remove what was opened.
        removable: bool,
    }

    impl Folder {
        /// Opens what `path` names — never what a link in the last part of
        /// the path points at — and requires that it lies where the path
        /// says.
        ///
        /// # Errors
        ///
        /// [`Refusal::Elsewhere`] when a link was followed on the way or the
        /// path cannot be compared with a real location; [`Refusal::Io`] when
        /// the path cannot be opened or asked about.
        pub(in super::super) fn open(path: &Path, hold: Hold) -> Result<Self, Refusal> {
            let (handle, removable) = match hold {
                Hold::Shared => (open_entry(path, FILE_READ_ATTRIBUTES.0, SHARE_ALL)?, false),
                Hold::ForRemoval => open_held(path)?,
            };
            let kind = kind_of(&handle)?;
            let real = final_path(&handle, VOLUME_NAME_DOS)?;
            let asked: Vec<u16> = path.as_os_str().encode_wide().collect();
            // A real location on a drive letter reads `\\?\X:\...`. Anything
            // else — a share (`\\?\UNC\...`), a volume without a letter — is
            // not a location that a request names.
            let (Some(location), Some(asked)) = (clean(&real), clean(&asked)) else {
                return Err(Refusal::Elsewhere);
            };
            if !same_text(&location, &asked) {
                // The request may spell a part by its short name (`PROGRA~1`).
                // Still different means that the path led somewhere else.
                let Ok(long) = long_names(&asked) else {
                    return Err(Refusal::Elsewhere);
                };
                if !same_text(&location, &long) {
                    return Err(Refusal::Elsewhere);
                }
            }
            Ok(Self {
                handle,
                kind,
                path: PathBuf::from(OsString::from_wide(&real)),
                // `X:\` and nothing after it.
                drive: location.len() == 3,
                removable,
            })
        }

        /// What was opened.
        pub(in super::super) fn kind(&self) -> Kind {
            self.kind
        }

        /// Where the folder really lies, in the verbatim form
        /// (`\\?\C:\...`). A path built from it names an entry by exactly
        /// that text: Win32 rewrites nothing in it, so a name that ends in a
        /// dot or a space, the name of a device (`nul`) and a very long path
        /// are plain names.
        pub(in super::super) fn path(&self) -> &Path {
            &self.path
        }

        /// Whether this is the root of a drive rather than a folder on one.
        pub(in super::super) fn is_drive(&self) -> bool {
            self.drive
        }

        /// What tells this folder apart from every other for as long as it
        /// exists, or `None` when its file system offers nothing of the kind.
        pub(in super::super) fn identity(&self) -> Option<Vec<u8>> {
            identity_of(&self.handle)
        }

        /// Where the folder really lies in the device form that every volume
        /// has, with every link on the way resolved.
        pub(in super::super) fn place(&self) -> io::Result<Vec<u16>> {
            place_of(&self.handle)
        }

        /// Removes the folder, which must be empty, or the link that was
        /// opened in its place, through its handle.
        ///
        /// A folder that could be held but not opened with the right to
        /// remove it — another program has it open, as its working folder
        /// for example — is tried once more now that the work in it is done.
        /// That takes a second open, which the hold itself would refuse, so
        /// the hold is given up first; the entry opened then is removed only
        /// if it is the one that was held.
        pub(in super::super) fn remove(self) -> io::Result<()> {
            if self.removable {
                return delete(&self.handle);
            }
            let identity = identity_of(&self.handle);
            let place = place_of(&self.handle)?;
            let Self {
                handle, kind, path, ..
            } = self;
            drop(handle);
            let again = open_for_removal(&path, SHARE_ALL)?;
            let same = identity.is_some()
                && identity_of(&again) == identity
                && kind_of(&again)? == kind
                && place_of(&again)? == place;
            if !same {
                // What lies at the path now is not what was held.
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            delete(&again)
        }
    }

    /// How [`open_at`] treats a name that does or does not exist.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(in super::super) enum Disposition {
        /// Opens the entry of that name; fails when there is none.
        Open,
        /// Creates a file of that name; fails when the name is taken, by
        /// whatever.
        Create,
    }

    impl Disposition {
        fn code(self) -> NTCREATEFILE_CREATE_DISPOSITION {
            match self {
                Self::Open => FILE_OPEN,
                Self::Create => FILE_CREATE,
            }
        }
    }

    /// Opens the entry `name` **in the opened folder**: the name is looked up
    /// in the directory the handle stands for, not by a path, so nothing that
    /// happens to a path meanwhile leads the open elsewhere. A link under
    /// that name is opened as the link, never followed.
    ///
    /// `access`, `share` and `options` are the access rights, the share mode
    /// and further create options of `NtCreateFile`; the handle is always
    /// opened for synchronous use. A new file gets the permissions the folder
    /// hands down.
    ///
    /// # Errors
    ///
    /// The error of the system, with the Windows error code that
    /// corresponds to the native status, so that it is classified like the
    /// error of any other call. A name that is not one plain name is refused
    /// before the system is asked.
    pub(in super::super) fn open_at(
        folder: &Folder,
        name: &str,
        access: u32,
        share: u32,
        disposition: Disposition,
        options: u32,
    ) -> io::Result<File> {
        let name = single_name(name).ok_or_else(not_a_name)?;
        let name_bytes = u16::try_from(name.len() * size_of::<u16>()).map_err(|_| not_a_name())?;
        let object_name = UNICODE_STRING {
            Length: name_bytes,
            MaximumLength: name_bytes,
            // The call reads the name and does not write to it.
            Buffer: PWSTR(name.as_ptr().cast_mut()),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of_u32::<OBJECT_ATTRIBUTES>(),
            RootDirectory: HANDLE(folder.handle.as_raw_handle()),
            ObjectName: &raw const object_name,
            // As Win32 opens: without regard to the case of the name.
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut opened = HANDLE::default();
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: every pointer is to a local value that lives through the
        // call: the two outputs are writable, `attributes` is initialized
        // with its own size, and it points to `object_name`, whose buffer is
        // `name` with exactly the length given. The root directory handle is
        // open for as long as `folder` is borrowed. No allocation size and no
        // extended attributes are passed.
        let status = unsafe {
            NtCreateFile(
                &raw mut opened,
                FILE_ACCESS_RIGHTS(access | SYNCHRONIZE.0),
                &raw const attributes,
                &raw mut status_block,
                None,
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_MODE(share),
                disposition.code(),
                FILE_OPEN_REPARSE_POINT
                    | FILE_SYNCHRONOUS_IO_NONALERT
                    | NTCREATEFILE_CREATE_OPTIONS(options),
                None,
                0,
            )
        };
        if status.is_err() {
            return Err(status_error(status));
        }
        // SAFETY: the call succeeded, so `opened` is an open file handle that
        // nothing else owns; the `File` closes it when it is dropped.
        Ok(unsafe { File::from_raw_handle(opened.0) })
    }

    /// Gives the opened file the name `name` **in the directory it lies
    /// in**, whatever happens to any path meanwhile. With `replace` a file
    /// of that name is replaced; without it a taken name fails the rename,
    /// in one step with the check.
    ///
    /// The handle must carry the right to delete.
    ///
    /// # Errors
    ///
    /// The error of the system, as for [`open_at`].
    pub(in super::super) fn rename_in_folder(
        entry: &File,
        name: &str,
        replace: bool,
    ) -> io::Result<()> {
        let name = single_name(name).ok_or_else(not_a_name)?;
        if !replace {
            // The plain form, which every Windows and every file system has:
            // a taken name fails the rename.
            return rename_with(entry, &name, FileRenameInformation, PLAIN_KEEP);
        }
        // To replace, the form of Windows 10 1607 and later: the name changes
        // at once, also when the file that is replaced is still open
        // elsewhere in a way that allows its removal. It is what the standard
        // library's own rename asks for.
        let flags = FILE_RENAME_POSIX_SEMANTICS | FILE_RENAME_REPLACE_IF_EXISTS;
        match rename_with(entry, &name, FileRenameInformationEx, flags) {
            // An older Windows, or a file system without the extended form:
            // the plain one.
            Err(error) if not_offered(&error) => {
                rename_with(entry, &name, FileRenameInformation, PLAIN_REPLACE)
            }
            renamed => renamed,
        }
    }

    /// One native rename of an opened file to the bare name `name`.
    fn rename_with(
        entry: &File,
        name: &[u16],
        class: FILE_INFORMATION_CLASS,
        flags: u32,
    ) -> io::Result<()> {
        let name_bytes = size_of_val(name);
        let size = size_of::<FILE_RENAME_INFORMATION>() + name_bytes;
        let (Ok(name_len), Ok(length)) = (u32::try_from(name_bytes), u32::try_from(size)) else {
            return Err(not_a_name());
        };
        // The structure ends in the name, so it is built in a buffer of its
        // own size plus the name's, zeroed and aligned for it.
        let mut buffer = vec![0u64; size.div_ceil(size_of::<u64>())];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        // SAFETY: `buffer` holds at least `size` bytes and is aligned for the
        // structure (asserted above), so the three header fields lie inside
        // it, and so do the `name.len()` units written from `FileName` on:
        // the field starts before the end of the structure. The writes go
        // through raw pointers; no reference to the structure is made. No
        // root directory is given: with a bare name the file is renamed
        // inside the directory it lies in.
        unsafe {
            (&raw mut (*info).Anonymous).write(FILE_RENAME_INFORMATION_0 { Flags: flags });
            (&raw mut (*info).RootDirectory).write(HANDLE::default());
            (&raw mut (*info).FileNameLength).write(name_len);
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                (&raw mut (*info).FileName).cast::<u16>(),
                name.len(),
            );
        }
        let mut status_block = IO_STATUS_BLOCK::default();
        // SAFETY: the handle is open for as long as `entry` is borrowed;
        // `info` points to `length` initialized bytes in `buffer`, which
        // lives through the call and is only read; the status block is a
        // writable local value.
        let status = unsafe {
            NtSetInformationFile(
                HANDLE(entry.as_raw_handle()),
                &raw mut status_block,
                info.cast::<c_void>().cast_const(),
                length,
                class,
            )
        };
        if status.is_err() {
            return Err(status_error(status));
        }
        Ok(())
    }

    /// Opens the entry at `path` itself — never what a link there points at —
    /// with the given access and share mode.
    pub(in super::super) fn open_entry(path: &Path, access: u32, share: u32) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(access)
            .share_mode(share)
            // `BACKUP_SEMANTICS` lets a folder be opened; `OPEN_REPARSE_POINT`
            // opens a link instead of following it.
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
    }

    /// Opens the entry at `path` for removal. The right to change attributes
    /// is asked for as well, so that a read-only entry can be made removable
    /// where the file system needs that; an entry that does not grant it is
    /// opened with the right to delete alone.
    pub(in super::super) fn open_for_removal(path: &Path, share: u32) -> io::Result<File> {
        let wanted = DELETE.0 | FILE_READ_ATTRIBUTES.0;
        match open_entry(path, wanted | FILE_WRITE_ATTRIBUTES.0, share) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                open_entry(path, wanted, share)
            }
            opened => opened,
        }
    }

    /// Opens a folder that is to be removed and holds it, so that nobody else
    /// renames or removes it meanwhile: with the right to remove it where
    /// that is granted, and otherwise with the right to list it. Returns the
    /// handle and whether it carries the right to remove.
    ///
    /// # Errors
    ///
    /// When the folder can be held in neither way: it is not taken up.
    fn open_held(path: &Path) -> io::Result<(File, bool)> {
        match open_for_removal(path, SHARE_HELD) {
            Ok(handle) => Ok((handle, true)),
            // The right to remove is denied, or another program has the
            // folder open in a way that excludes its removal: the folder is
            // held with the right to list it, and its content can still be
            // removed.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied || excluded(&error) => {
                open_entry(path, TO_HOLD, SHARE_HELD).map(|handle| (handle, false))
            }
            Err(error) => Err(error),
        }
    }

    /// What an opened entry is. The standard library reads it from the handle
    /// and calls a reparse point a symbolic link exactly when its tag marks it
    /// as standing for another place (a symbolic link, a junction, a volume
    /// mount point); a folder with any other reparse tag is a folder.
    pub(in super::super) fn kind_of(entry: &File) -> io::Result<Kind> {
        let kind = entry.metadata()?.file_type();
        Ok(if kind.is_symlink() {
            Kind::Link
        } else if kind.is_dir() {
            Kind::Dir
        } else {
            Kind::File
        })
    }

    /// Where an opened entry really lies: its final path, with every link on
    /// the way resolved, in the device form that every volume has.
    pub(in super::super) fn place_of(entry: &File) -> io::Result<Vec<u16>> {
        final_path(entry, VOLUME_NAME_NT)
    }

    /// The final path of an opened entry in the given form of its volume.
    fn final_path(entry: &File, volume: GETFINALPATHNAMEBYHANDLE_FLAGS) -> io::Result<Vec<u16>> {
        let handle = HANDLE(entry.as_raw_handle());
        let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | volume.0);
        path_from(|buffer| {
            // SAFETY: the handle is open for as long as `entry` is borrowed,
            // and `buffer` is a writable buffer whose length the wrapper
            // passes with it.
            unsafe { GetFinalPathNameByHandleW(handle, buffer, flags) }
        })
    }

    /// The path `asked`, which is rooted on a drive and has no prefix, with
    /// every part that is spelled by its short name replaced by the long
    /// one.
    fn long_names(asked: &[u16]) -> io::Result<Vec<u16>> {
        // A short path is given to the system as it is. A long one needs the
        // verbatim prefix to be accepted, and so does one whose long form is
        // long: the call is then tried again that way.
        if asked.len() < PLAIN_PATH_UNITS
            && let Ok(long) = expand_short_names(asked, false)
        {
            return Ok(long);
        }
        expand_short_names(asked, true)
    }

    /// One call for the long names of `asked`, given to the system with or
    /// without the verbatim prefix and returned without it.
    fn expand_short_names(asked: &[u16], prefixed: bool) -> io::Result<Vec<u16>> {
        let mut short = Vec::with_capacity(VERBATIM.len() + asked.len() + 1);
        if prefixed {
            short.extend_from_slice(&VERBATIM);
        }
        short.extend_from_slice(asked);
        short.push(0);
        let long = path_from(|buffer| {
            // SAFETY: `short` is a NUL-terminated UTF-16 string that lives
            // through the call, and `buffer` is a writable buffer whose
            // length the wrapper passes with it.
            unsafe { GetLongPathNameW(PCWSTR(short.as_ptr()), Some(buffer)) }
        })?;
        Ok(match long.strip_prefix(&VERBATIM) {
            Some(location) if prefixed => location.to_vec(),
            _ => long,
        })
    }

    /// Runs a Win32 call that writes a path into a buffer and answers the
    /// length of the path, or the size the buffer must have, or zero when it
    /// failed.
    fn path_from(mut fill: impl FnMut(&mut [u16]) -> u32) -> io::Result<Vec<u16>> {
        let mut path = vec![0u16; 512];
        loop {
            let len = fill(&mut path) as usize;
            if len == 0 {
                return Err(io::Error::last_os_error());
            }
            // A length below the buffer's is the path, without its
            // terminator; anything else is the size the buffer must have.
            if len < path.len() {
                path.truncate(len);
                return Ok(path);
            }
            if len > MAX_PATH_UNITS {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            path.resize(len + 1, 0);
        }
    }

    /// Whether two paths are the same text, without regard to the case of
    /// their letters, as the system compares names.
    fn same_text(one: &[u16], other: &[u16]) -> bool {
        // SAFETY: both arguments are slices, whose pointers and lengths the
        // wrapper passes together; the call only reads them.
        unsafe { CompareStringOrdinal(one, other, true) == CSTR_EQUAL }
    }

    /// What tells an opened entry apart from every other on this computer
    /// for as long as it exists: its volume and its identifier in that
    /// volume.
    fn identity_of(entry: &File) -> Option<Vec<u8>> {
        let handle = HANDLE(entry.as_raw_handle());
        let mut id = FILE_ID_INFO::default();
        // SAFETY: the handle is open for as long as `entry` is borrowed, and
        // the pointer is to a writable value of the structure this class
        // fills, passed with its size.
        let asked = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&raw mut id).cast::<c_void>(),
                size_of_u32::<FILE_ID_INFO>(),
            )
        };
        if asked.is_ok() {
            let mut identity = Vec::with_capacity(25);
            identity.push(VOLUME_AND_FILE_ID);
            identity.extend_from_slice(&id.VolumeSerialNumber.to_le_bytes());
            identity.extend_from_slice(&id.FileId.Identifier);
            return Some(identity);
        }
        // A file system without the long identifier (FAT, exFAT).
        let facts = handle_facts(entry).ok()?;
        let mut identity = Vec::with_capacity(13);
        identity.push(VOLUME_AND_FILE_INDEX);
        identity.extend_from_slice(&facts.dwVolumeSerialNumber.to_le_bytes());
        identity.extend_from_slice(&facts.nFileIndexHigh.to_le_bytes());
        identity.extend_from_slice(&facts.nFileIndexLow.to_le_bytes());
        Some(identity)
    }

    /// How many names an opened file has. A file with more than one is also
    /// reachable under a name somewhere else (a hard link).
    pub(in super::super) fn link_count(entry: &File) -> io::Result<u32> {
        Ok(handle_facts(entry)?.nNumberOfLinks)
    }

    /// The basic facts of an opened entry.
    fn handle_facts(entry: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut facts = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle is open for as long as `entry` is borrowed, and
        // the pointer is to a writable value of the structure the call fills.
        unsafe { GetFileInformationByHandle(HANDLE(entry.as_raw_handle()), &raw mut facts) }
            .map_err(|error| os_error(&error))?;
        Ok(facts)
    }

    /// Removes an opened entry through its handle, which must carry the right
    /// to delete.
    pub(in super::super) fn delete(entry: &File) -> io::Result<()> {
        let handle = HANDLE(entry.as_raw_handle());
        // The form of Windows 10 1809 and later: the name is gone at once,
        // even while others still hold the entry open, and a read-only entry
        // is removed like any other.
        let extended = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_INFO_EX_FLAGS(
                FILE_DISPOSITION_FLAG_DELETE.0
                    | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0
                    | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE.0,
            ),
        };
        // SAFETY: the handle is open for as long as `entry` is borrowed, and
        // the pointer is to an initialized value of the structure this class
        // takes, with its size, read only during the call.
        let removed = unsafe {
            SetFileInformationByHandle(
                handle,
                FileDispositionInfoEx,
                std::ptr::from_ref(&extended).cast::<c_void>(),
                size_of_u32::<FILE_DISPOSITION_INFO_EX>(),
            )
        };
        let error = match removed {
            Ok(()) => return Ok(()),
            Err(error) => os_error(&error),
        };
        if !not_offered(&error) {
            return Err(error);
        }
        // An older Windows, or a file system without the extended form (FAT,
        // exFAT): a read-only entry is made removable first, then the entry
        // is marked for removal the plain way.
        let facts = entry.metadata()?;
        let mut permissions = facts.permissions();
        if permissions.readonly() {
            #[allow(
                clippy::permissions_set_readonly_false,
                reason = "Windows only: this clears the read-only attribute of an entry that is being removed"
            )]
            permissions.set_readonly(false);
            entry.set_permissions(permissions)?;
        }
        let plain = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: as above, for the plain class and its structure.
        unsafe {
            SetFileInformationByHandle(
                handle,
                FileDispositionInfo,
                std::ptr::from_ref(&plain).cast::<c_void>(),
                size_of_u32::<FILE_DISPOSITION_INFO>(),
            )
        }
        .map_err(|error| os_error(&error))
    }

    /// Whether an error says that the extended form of a call is not offered
    /// here, so that the plain form is to be used.
    fn not_offered(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
        )
    }

    /// Whether an error says that another handle excludes this use.
    fn excluded(error: &io::Error) -> bool {
        matches!(
            error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
        )
    }

    /// The error for a name that is not one plain name in a folder.
    fn not_a_name() -> io::Error {
        io::Error::from(io::ErrorKind::InvalidInput)
    }

    /// The size of `T` as the `u32` the calls take.
    fn size_of_u32<T>() -> u32 {
        u32::try_from(size_of::<T>()).unwrap_or(0)
    }

    /// The I/O error for a failed native call: the Windows error code that
    /// corresponds to its status, so that it is classified like the error of
    /// any other call.
    fn status_error(status: NTSTATUS) -> io::Error {
        // Win32 reports "this is a folder" as "access denied". It is said as
        // what it is, so that a folder where a file was expected is not taken
        // for a matter of permissions.
        if status == STATUS_FILE_IS_A_DIRECTORY {
            return io::Error::from(io::ErrorKind::IsADirectory);
        }
        // SAFETY: the call takes a plain value and answers a plain value.
        let code = unsafe { RtlNtStatusToDosError(status) };
        io::Error::from_raw_os_error(code.cast_signed())
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
            io::Error::other("the file operation failed")
        }
    }
}

#[cfg(test)]
mod tests;
