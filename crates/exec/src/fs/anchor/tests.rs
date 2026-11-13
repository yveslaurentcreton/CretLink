//! Tests of the verified folder (ST-M010-001).
//!
//! The lexical rules are plain functions and are tested on every platform.
//! What a folder is and what tells it apart from another are tested against a
//! real, sandboxed file system. What only Windows verifies — where an opened
//! folder really lies, and that an open and a rename stay inside an opened
//! folder — is covered by the tests marked `cfg(windows)`, which run on the
//! Windows CI runner.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Folder, Hold, Kind, Refusal, clean, plain, single_name};

/// A unique temporary directory, removed when the guard drops.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("cretlink-anchor-test-{pid}-{n}"));
        std::fs::create_dir_all(&root).expect("create the sandbox");
        Self { root }
    }

    /// Creates the folder `name` in the sandbox and returns it.
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).expect("create a folder");
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The cleaned form of a path given as text.
fn cleaned(asked: &str) -> Option<String> {
    let units: Vec<u16> = asked.encode_utf16().collect();
    clean(&units).map(|units| String::from_utf16(&units).expect("the cleaned path is text"))
}

// --- the lexical rules ------------------------------------------------------

#[test]
fn a_requested_path_is_brought_into_the_form_of_a_real_location() {
    assert_eq!(
        cleaned(r"C:\Users\ann\Downloads").as_deref(),
        Some(r"C:\Users\ann\Downloads")
    );
    // Either separator, a doubled one and one at the end.
    assert_eq!(
        cleaned("C:/Users/ann//Downloads/").as_deref(),
        Some(r"C:\Users\ann\Downloads")
    );
    assert_eq!(cleaned(r"c:\\Temp\\\old\").as_deref(), Some(r"c:\Temp\old"));
    // The root of a drive keeps its separator, and only that one.
    for root in [r"C:\", "C:/", r"C:\\", r"C:\/"] {
        assert_eq!(cleaned(root).as_deref(), Some(r"C:\"), "{root}");
    }
    // A name may hold dots and spaces, just not at its end.
    assert_eq!(
        cleaned(r"D:\My Files\.config\report.v2.docx").as_deref(),
        Some(r"D:\My Files\.config\report.v2.docx")
    );
}

/// Win32 rewrites some paths before it opens anything: it drops a dot or a
/// space at the end of a part and resolves `.` and `..`. The text of such a
/// path is not the location it opens, so there is nothing to compare a real
/// location with, and the path is refused.
#[test]
fn a_path_that_win32_would_rewrite_has_no_location_to_compare() {
    for rewritten in [
        r"C:\Temp\folder.",
        r"C:\Temp\folder ",
        r"C:\Temp.\folder",
        r"C:\Temp \folder",
        r"C:\Temp\.\folder",
        r"C:\Temp\..\Windows",
        r"C:\Temp\...",
    ] {
        assert_eq!(cleaned(rewritten), None, "{rewritten}");
    }
}

#[test]
fn a_path_that_is_not_rooted_on_a_drive_letter_has_no_location_to_compare() {
    for unrooted in [
        "",
        "C:",
        "C:folder",
        r"\Temp\folder",
        r"relative\folder",
        r"\\server\share\folder",
        r"\\.\C:\Temp",
        r"1:\Temp",
        r"é:\Temp",
        "/home/ann",
    ] {
        assert_eq!(cleaned(unrooted), None, "{unrooted}");
    }
}

/// A verbatim path is what Win32 passes on as it is. A real location is read
/// from the system in that form.
#[test]
fn a_verbatim_path_is_taken_as_it_is_without_its_prefix() {
    assert_eq!(
        cleaned(r"\\?\C:\Users\ann\Downloads").as_deref(),
        Some(r"C:\Users\ann\Downloads")
    );
    assert_eq!(cleaned(r"\\?\C:\").as_deref(), Some(r"C:\"));
    // Nothing is rewritten in it: a dot at the end is part of the name, and
    // `/` is a character of a name, not a separator.
    assert_eq!(
        cleaned(r"\\?\C:\Temp\folder.").as_deref(),
        Some(r"C:\Temp\folder.")
    );
    assert_eq!(cleaned(r"\\?\C:\a/b").as_deref(), Some(r"C:\a/b"));
    assert_eq!(cleaned(r"\\?\C:/Temp"), None);
    assert_eq!(cleaned(r"\\?\C:\a\\b"), None);
    // A share and a volume without a drive letter are not locations that a
    // request can name.
    assert_eq!(cleaned(r"\\?\UNC\server\share\folder"), None);
    assert_eq!(
        cleaned(r"\\?\Volume{00000000-0000-0000-0000-000000000000}\folder"),
        None
    );
}

#[test]
fn a_name_in_a_folder_is_one_plain_name() {
    assert_eq!(
        single_name("report.docx"),
        Some("report.docx".encode_utf16().collect())
    );
    // A dot or a space at the end, and the name of a device, are plain names
    // for an open relative to a folder.
    for plain_name in [".cretlink-part.0011223344556677", "trailing.", "nul", "a b"] {
        assert!(single_name(plain_name).is_some(), "{plain_name}");
    }
    for not_a_name in [
        "",
        ".",
        "..",
        r"sub\file.txt",
        "sub/file.txt",
        r"..\outside",
        "file.txt:stream",
        "C:",
        "nul\0",
    ] {
        assert_eq!(single_name(not_a_name), None, "{not_a_name:?}");
    }
}

#[test]
fn the_text_of_a_path_is_given_without_the_verbatim_prefix() {
    let sb = Sandbox::new();
    let folder = sb.dir("plain");
    // A path as it was given is given back as it is.
    assert_eq!(plain(&folder), folder.display().to_string());

    #[cfg(windows)]
    {
        assert_eq!(
            plain(Path::new(r"\\?\C:\Temp\old\report.docx")),
            r"C:\Temp\old\report.docx"
        );
        assert_eq!(plain(Path::new(r"\\?\C:\")), r"C:\");
        // Only the prefix of a drive path is left out.
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share")),
            r"\\?\UNC\server\share"
        );
    }
}

// --- a folder that was taken up ---------------------------------------------

#[test]
fn a_folder_is_taken_up_as_a_folder_and_a_file_as_a_file() {
    let sb = Sandbox::new();
    let folder = sb.dir("folder");
    let file = sb.root.join("file.txt");
    std::fs::write(&file, b"a file").expect("write");

    for hold in [Hold::Shared, Hold::ForRemoval] {
        let opened = Folder::open(&folder, hold).expect("the folder is taken up");
        assert_eq!(opened.kind(), Kind::Dir);
        assert!(!opened.is_drive(), "a folder in the sandbox is not a drive");
        assert_eq!(
            plain(opened.path()),
            super::super::testing::reported(&folder)
        );

        let opened = Folder::open(&file, hold).expect("the file is taken up");
        assert_eq!(opened.kind(), Kind::File);
    }

    match Folder::open(&sb.root.join("not-there"), Hold::ForRemoval) {
        Err(Refusal::Io(error)) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected a not-found refusal, got {other:?}"),
    }
}

/// The identity of a folder is its own: it stays when the folder is renamed,
/// and another folder has another one — also one that takes over the name.
#[test]
fn the_identity_of_a_folder_follows_the_folder_not_its_name() {
    let sb = Sandbox::new();
    let first = sb.dir("first");
    let identity = |path: &Path| {
        Folder::open(path, Hold::Shared)
            .expect("the folder is taken up")
            .identity()
            .expect("this file system tells its folders apart")
    };

    let of_first = identity(&first);
    assert!(!of_first.is_empty());
    assert!(
        of_first.len() <= cretlink_protocol::limits::MAX_FOLDER_ID_BYTES,
        "an identity fits the wire: {} bytes",
        of_first.len()
    );
    assert_eq!(identity(&first), of_first, "the same folder, opened again");

    let moved = sb.root.join("moved");
    std::fs::rename(&first, &moved).expect("rename the folder");
    assert_eq!(
        identity(&moved),
        of_first,
        "the same folder under a new name"
    );

    // Another folder takes over the name while the first still exists.
    let second = sb.dir("first");
    assert_ne!(identity(&second), of_first, "another folder of that name");
}

#[test]
fn an_empty_folder_is_removed_through_its_hold_and_a_full_one_is_not() {
    let sb = Sandbox::new();
    let empty = sb.dir("empty");
    Folder::open(&empty, Hold::ForRemoval)
        .expect("the folder is taken up")
        .remove()
        .expect("an empty folder is removed");
    assert!(!empty.exists());

    let full = sb.dir("full");
    std::fs::write(full.join("kept.txt"), b"kept").expect("write");
    let error = Folder::open(&full, Hold::ForRemoval)
        .expect("the folder is taken up")
        .remove()
        .expect_err("a folder that holds something is not removed");
    assert_eq!(
        super::super::io_error_code(&error),
        cretlink_protocol::ErrorCode::DirNotEmpty
    );
    assert!(full.join("kept.txt").exists());
}

/// A link in the place of a folder is taken up as the link it is, never as
/// what it points at, and removing it leaves that untouched.
#[cfg(unix)]
#[test]
fn a_link_is_taken_up_as_a_link() {
    let sb = Sandbox::new();
    let real = sb.dir("real");
    std::fs::write(real.join("inner.txt"), b"inner").expect("write");
    let link = sb.root.join("link");
    std::os::unix::fs::symlink(&real, &link).expect("link to the folder");

    let opened = Folder::open(&link, Hold::ForRemoval).expect("the link is taken up");
    assert_eq!(opened.kind(), Kind::Link);
    opened.remove().expect("the link is removed");
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the link is gone"
    );
    assert!(real.join("inner.txt").exists(), "what it pointed at is not");
}

// --- Windows only -----------------------------------------------------------

#[cfg(windows)]
mod windows {
    use std::io::{Read, Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    use cretlink_protocol::ErrorCode;

    use super::super::super::io_error_code;
    use super::super::super::testing::{junction, reported, short_name};
    use super::super::{
        Disposition, Folder, Hold, Kind, Refusal, link_count, open_at, plain, rename_in_folder,
    };
    use super::Sandbox;

    // Access rights and share modes of a file (winnt.h).
    const FILE_READ_DATA: u32 = 0x0001;
    const FILE_WRITE_DATA: u32 = 0x0002;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const FILE_TRAVERSE: u32 = 0x0020;
    const DELETE: u32 = 0x0001_0000;
    const SHARE_ALL: u32 = 0x0007;

    /// Everything a test does with a file it opens in a folder.
    const EVERYTHING: u32 = FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | DELETE;

    fn shared(path: &Path) -> Folder {
        Folder::open(path, Hold::Shared).expect("the folder is taken up")
    }

    /// A path that reaches its folder through a junction names a place the
    /// folder does not lie in. It is refused, with either hold.
    #[test]
    fn a_folder_reached_through_a_junction_is_refused() {
        let sb = Sandbox::new();
        let real = sb.dir("real");
        std::fs::create_dir(real.join("inside")).expect("create");
        let via = sb.root.join("via");
        junction(&via, &real);

        for hold in [Hold::Shared, Hold::ForRemoval] {
            match Folder::open(&via.join("inside"), hold) {
                Err(Refusal::Elsewhere) => {}
                other => panic!("expected a refusal, got {other:?}"),
            }
            // The same folder by its own path is taken up.
            let opened = Folder::open(&real.join("inside"), hold).expect("its real path");
            assert_eq!(opened.kind(), Kind::Dir);
        }
    }

    /// The junction itself, as the last part of the path, is opened as the
    /// link it is and lies where the path says. What it means there is the
    /// caller's to decide.
    #[test]
    fn a_junction_in_the_last_place_is_taken_up_as_a_link() {
        let sb = Sandbox::new();
        let real = sb.dir("real");
        let link = sb.root.join("link");
        junction(&link, &real);

        let opened = Folder::open(&link, Hold::Shared).expect("the junction is taken up");
        assert_eq!(opened.kind(), Kind::Link);
        assert_eq!(plain(opened.path()), reported(&sb.root) + r"\link");
    }

    /// The path may be spelled in another letter case, with `/`, with a
    /// separator at its end, or by its short names: it still names the place
    /// the folder lies in. The verified location is the one the disk has.
    #[test]
    fn another_spelling_of_the_real_location_is_accepted() {
        let sb = Sandbox::new();
        let folder = sb.dir("Mixed Case Folder");
        let real = reported(&folder);
        let text = folder.display().to_string();

        let mut spellings = vec![
            text.to_uppercase(),
            text.to_lowercase(),
            text.replace('\\', "/"),
            format!("{text}\\"),
        ];
        match short_name(&folder) {
            Some(short) => spellings.push(short),
            None => eprintln!("skipped the short names: this volume keeps none"),
        }
        for spelled in spellings {
            let opened = Folder::open(Path::new(&spelled), Hold::Shared)
                .unwrap_or_else(|refusal| panic!("{spelled} is refused: {refusal:?}"));
            assert_eq!(plain(opened.path()), real, "{spelled}");
        }
    }

    /// The root of a drive is taken up and known as one.
    #[test]
    fn the_root_of_a_drive_is_known_as_a_drive() {
        let sb = Sandbox::new();
        let drive = &reported(&sb.root)[..3];
        for spelled in [
            drive.to_owned(),
            drive.to_lowercase(),
            drive.replace('\\', "/"),
        ] {
            let opened = Folder::open(Path::new(&spelled), Hold::Shared).expect("the drive");
            assert!(opened.is_drive(), "{spelled}");
            assert_eq!(opened.kind(), Kind::Dir);
            assert_eq!(plain(opened.path()), drive);
        }
    }

    /// A name is opened in the opened folder itself. The folder can be
    /// renamed away and a junction put in its place: the open still lands in
    /// the folder that was opened, and never where the path leads by then.
    #[test]
    fn an_open_in_a_folder_stays_in_that_folder_whatever_its_path_leads_to() {
        let sb = Sandbox::new();
        let folder = sb.dir("folder");
        let elsewhere = sb.dir("elsewhere");
        let opened = shared(&folder);

        let moved = sb.root.join("moved");
        std::fs::rename(&folder, &moved).expect("rename the folder that is open");
        junction(&folder, &elsewhere);

        let mut file = open_at(
            &opened,
            "note.txt",
            EVERYTHING,
            SHARE_ALL,
            Disposition::Create,
            0,
        )
        .expect("create a file in the opened folder");
        file.write_all(b"in the folder").expect("write");
        drop(file);

        assert_eq!(
            std::fs::read(moved.join("note.txt")).expect("read"),
            b"in the folder"
        );
        assert!(
            !elsewhere.join("note.txt").exists(),
            "nothing went where the path leads now"
        );
    }

    #[test]
    fn an_open_in_a_folder_opens_creates_and_refuses_as_asked() {
        let sb = Sandbox::new();
        let folder = sb.dir("folder");
        std::fs::write(folder.join("there.txt"), b"there").expect("write");
        std::fs::create_dir(folder.join("sub")).expect("create");
        let opened = shared(&folder);
        let open = |name: &str, disposition, options| {
            open_at(&opened, name, EVERYTHING, SHARE_ALL, disposition, options)
        };

        let mut content = String::new();
        open("there.txt", Disposition::Open, 0)
            .expect("an existing file is opened")
            .read_to_string(&mut content)
            .expect("read");
        assert_eq!(content, "there");
        // Without regard to the case of the name, as Win32 opens.
        open("THERE.TXT", Disposition::Open, 0).expect("in another letter case");

        let missing = open("missing.txt", Disposition::Open, 0).expect_err("nothing to open");
        assert_eq!(io_error_code(&missing), ErrorCode::PathNotFound);
        let taken = open("there.txt", Disposition::Create, 0).expect_err("the name is taken");
        assert_eq!(io_error_code(&taken), ErrorCode::AlreadyExists);

        // FILE_NON_DIRECTORY_FILE: a folder under the name is said to be one.
        const FILE_NON_DIRECTORY_FILE: u32 = 0x0040;
        let folder_there = open("sub", Disposition::Open, FILE_NON_DIRECTORY_FILE)
            .expect_err("a folder is not opened as a file");
        assert_eq!(io_error_code(&folder_there), ErrorCode::WrongPathKind);

        // A name that is a path is refused before the system is asked.
        for not_a_name in ["", ".", "..", r"sub\x", "sub/x", r"..\x", "there.txt:s"] {
            assert!(
                open(not_a_name, Disposition::Create, 0).is_err(),
                "{not_a_name:?}"
            );
        }
        let mut names: Vec<String> = std::fs::read_dir(&folder)
            .expect("list")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, ["sub", "there.txt"], "nothing else was created");
    }

    /// A link under a name is opened as the link, never as what it points
    /// at. A junction needs no privilege, so the test runs everywhere.
    #[test]
    fn a_link_under_a_name_is_opened_as_the_link() {
        let sb = Sandbox::new();
        let folder = sb.dir("folder");
        let target = sb.dir("target");
        junction(&folder.join("link"), &target);
        let opened = shared(&folder);

        let link = open_at(
            &opened,
            "link",
            FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            Disposition::Open,
            0,
        )
        .expect("the link is opened");
        assert_eq!(
            super::super::kind_of(&link).expect("what it is"),
            Kind::Link
        );
    }

    /// The rename that publishes a file gives it its name in the folder it
    /// lies in — not in the current directory of the process, where a bare
    /// name would be looked for as a path — and replaces only when asked.
    #[test]
    fn a_rename_through_a_handle_stays_in_the_folder_of_the_file() {
        let sb = Sandbox::new();
        let folder = sb.dir("folder");
        let opened = shared(&folder);
        let create = |name: &str, content: &[u8]| {
            let mut file = open_at(&opened, name, EVERYTHING, SHARE_ALL, Disposition::Create, 0)
                .expect("create");
            file.write_all(content).expect("write");
            file
        };
        // A name that no other test and nothing in the current directory has.
        let published = format!("published-{}.txt", std::process::id());
        let here = std::env::current_dir().expect("a current directory");

        let part = create("part-1", b"first");
        rename_in_folder(&part, &published, false).expect("the name is free");
        drop(part);
        assert_eq!(
            std::fs::read(folder.join(&published)).expect("read"),
            b"first"
        );
        assert!(!folder.join("part-1").exists());
        assert!(
            !here.join(&published).exists(),
            "the file did not go to the current directory"
        );

        // A taken name fails the rename; the file under it is untouched.
        let part = create("part-2", b"second");
        let taken = rename_in_folder(&part, &published, false).expect_err("the name is taken");
        assert_eq!(io_error_code(&taken), ErrorCode::AlreadyExists);
        assert_eq!(
            std::fs::read(folder.join(&published)).expect("read"),
            b"first"
        );
        // On request it is replaced.
        rename_in_folder(&part, &published, true).expect("replace");
        drop(part);
        assert_eq!(
            std::fs::read(folder.join(&published)).expect("read"),
            b"second"
        );
        assert!(!folder.join("part-2").exists());

        // A name that is a path is refused: a rename through a handle never
        // moves a file out of its folder.
        let part = create("part-3", b"third");
        for not_a_name in [r"..\outside.txt", r"C:\outside.txt", "sub/x", ""] {
            assert!(
                rename_in_folder(&part, not_a_name, true).is_err(),
                "{not_a_name:?}"
            );
        }
        drop(part);
        assert_eq!(
            std::fs::read(folder.join("part-3")).expect("read"),
            b"third"
        );
        assert!(!sb.root.join("outside.txt").exists());
    }

    /// A file that has a second name somewhere is known as such.
    #[test]
    fn the_names_of_a_file_are_counted() {
        let sb = Sandbox::new();
        let folder = sb.dir("folder");
        let file = folder.join("one.txt");
        std::fs::write(&file, b"one").expect("write");
        let open = || {
            std::fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(SHARE_ALL)
                .open(&file)
                .expect("open")
        };
        assert_eq!(link_count(&open()).expect("count"), 1);
        std::fs::hard_link(&file, sb.root.join("two.txt")).expect("a second name");
        assert_eq!(link_count(&open()).expect("count"), 2);
    }

    /// While a folder is held for removal nobody renames or removes it, and
    /// what is in it stays usable.
    #[test]
    fn a_folder_held_for_removal_cannot_be_renamed_or_removed_by_others() {
        let sb = Sandbox::new();
        let folder = sb.dir("held");
        let held = Folder::open(&folder, Hold::ForRemoval).expect("the folder is taken up");

        std::fs::rename(&folder, sb.root.join("away")).expect_err("renamed while held");
        std::fs::remove_dir(&folder).expect_err("removed while held");
        std::fs::write(folder.join("inside.txt"), b"inside").expect("write in it");
        std::fs::remove_file(folder.join("inside.txt")).expect("remove in it");

        held.remove().expect("the holder removes it");
        assert!(!folder.exists());
    }

    /// A folder that another program has open in a way that excludes its
    /// removal — as its working folder — is still taken up; the removal is
    /// tried again at the end and says that the folder is in use.
    #[test]
    fn a_folder_another_program_holds_is_taken_up_and_reported_in_use() {
        // As a program opens its working folder: with the right to pass
        // through it, shared for reading and writing but not for removal
        // (FILE_SHARE_READ | FILE_SHARE_WRITE), FILE_FLAG_BACKUP_SEMANTICS.
        // The right matters: Windows applies the share mode of a handle only
        // when the handle may read, write or delete, so a handle that may
        // only read attributes would exclude nothing.
        let other = |path: &Path| {
            std::fs::OpenOptions::new()
                .access_mode(FILE_TRAVERSE)
                .share_mode(0x0003)
                .custom_flags(0x0200_0000)
                .open(path)
                .expect("another program opens the folder")
        };
        let sb = Sandbox::new();
        let folder = sb.dir("in-use");

        let theirs = other(&folder);
        let held = Folder::open(&folder, Hold::ForRemoval).expect("the folder is taken up");
        let error = held.remove().expect_err("in use by another program");
        assert_eq!(io_error_code(&error), ErrorCode::FileInUse);
        assert!(folder.exists());

        // Released while the folder is held: the removal at the end succeeds.
        let theirs_again = other(&folder);
        drop(theirs);
        let held = Folder::open(&folder, Hold::ForRemoval).expect("the folder is taken up");
        drop(theirs_again);
        held.remove().expect("free by now");
        assert!(!folder.exists());
    }
}
