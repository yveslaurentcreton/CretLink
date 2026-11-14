//! Tests of the folders CretLink keeps for itself (ST-M006-020).
//!
//! The rule about what private permissions look like, the chain of folders
//! that are prepared, and what happens when something else stands in the
//! place of a folder are tested on every platform; off Windows "something
//! else" is a link or an entry of another kind. The owner and the permissions
//! are Windows facts and are tested there, on folders in a sandbox and, in
//! continuous verification, on the real place in `Program Files`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    OwnFolder, Standing, administrators_folder, earlier_release_folder, earlier_release_in_use,
    is_private_text, is_program_folder_text, kept_program_folder, open_webview_profile,
    prepare_program_folder, remove_whole, webview_profile_descriptor,
};

/// A unique temp directory removed when the guard drops.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("cretlink-own-test-{pid}-{n}"));
        std::fs::create_dir_all(&root).expect("create sandbox root");
        Self { root }
    }

    /// The folder that stands for the data folder of an elevated program.
    fn data(&self) -> PathBuf {
        self.root.join("CretLink Data")
    }

    /// The staging area in [`Self::data`], private from the data folder down.
    fn staging(&self) -> OwnFolder {
        OwnFolder::administrators_only(self.data(), self.data().join("staging"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
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
    names
}

/// Whether the tests of private folders can run here. Off Windows always; on
/// Windows only as an administrator, and always in continuous verification.
fn can_make_private() -> bool {
    #[cfg(windows)]
    {
        super::tests_can_name_the_administrators()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

// --- the rule about private permissions ---------------------------------------

#[test]
fn the_private_permissions_of_a_folder_are_recognised_in_the_forms_the_system_writes() {
    for text in [
        // As the folder is created.
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        // With the mark of a list that was computed with inheritance.
        "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        // The two entries in the other order, and the system as the owner.
        "O:SYD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)",
        // With a group, and with a list for auditing behind it.
        "O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)S:AI(AU;SA;FA;;;WD)",
        // Full control spelled as a number.
        "O:BAD:P(A;OICI;0x1f01ff;;;SY)(A;OICI;0x1F01FF;;;BA)",
    ] {
        assert!(is_private_text(text), "{text}");
    }
}

#[test]
fn anything_else_is_not_private() {
    for text in [
        // Not protected: the folder above can hand permissions down.
        "O:BAD:(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        "O:BAD:AI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        // Handed down from the folder above, not the folder's own.
        "O:BAD:PAI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)",
        // Someone else has a right, however small.
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x1200a9;;;BU)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICIIO;GA;;;CO)",
        "O:BAD:P(A;OICI;FA;;;WD)(A;OICI;FA;;;BA)",
        // One of the two is missing, or has less than full control, or what
        // it has is not handed down to what is made in the folder.
        "O:BAD:P(A;OICI;FA;;;BA)",
        "O:BAD:P(A;OICI;FA;;;SY)",
        "O:BAD:P(A;OICI;0x1301bf;;;SY)(A;OICI;FA;;;BA)",
        "O:BAD:P(A;;FA;;;SY)(A;OICI;FA;;;BA)",
        "O:BAD:P(A;CI;FA;;;SY)(A;OICI;FA;;;BA)",
        // The same one twice does not make two.
        "O:BAD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;BA)",
        // A refusal is not what CretLink sets.
        "O:BAD:P(D;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        // An entry for one kind of object only.
        "O:BAD:P(OA;OICI;FA;00000000-0000-0000-0000-000000000000;;SY)(A;OICI;FA;;;BA)",
        // Another owner: an account, everyone, nobody named.
        "O:S-1-5-21-1-2-3-1001D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        "O:WDD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        // No list at all, which lets everyone do everything; an empty one.
        "O:BAD:NO_ACCESS_CONTROL",
        "O:BAD:P",
        "O:BA",
        // Not a descriptor.
        "",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA",
        "O:BAD:P(A;OICI;FA;;SY)(A;OICI;FA;;;BA)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)x",
    ] {
        assert!(!is_private_text(text), "{text}");
    }
}

// --- the rule about the folder of a kept program (ST-M009-017) -----------------

#[test]
fn the_permissions_of_the_folder_of_a_kept_program_are_recognised() {
    for text in [
        // As the folder is created.
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
        // As the system writes it back, in any order and spelling.
        "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200A9;;;BU)",
        "O:SYD:P(A;OICI;0x1200a9;;;BU)(A;OICI;0x1f01ff;;;BA)(A;OICI;FA;;;SY)",
        "O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FXFR;;;BU)S:AI(AU;SA;FA;;;WD)",
    ] {
        assert!(is_program_folder_text(text), "{text}");
        // A folder that every account may read is not a private one.
        assert!(!is_private_text(text), "{text}");
    }
}

#[test]
fn anything_else_is_not_the_folder_of_a_kept_program() {
    for text in [
        // A private folder: no account could start the program in it.
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
        // The accounts may do more than read and start: write, change,
        // everything.
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;BU)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1201bf;;;BU)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;BU)",
        // Someone else beside them, however little that one may do.
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)(A;OICI;0x1200a9;;;WD)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)(A;OICIIO;GA;;;CO)",
        // Everyone in the place of the accounts of this computer.
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;WD)",
        // Not protected, handed down from above, or not handed down to what
        // is made in the folder.
        "O:BAD:(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
        "O:BAD:PAI(A;OICIID;FA;;;SY)(A;OICIID;FA;;;BA)(A;OICIID;0x1200a9;;;BU)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x1200a9;;;BU)",
        // The administrators or the system missing, or with less.
        "O:BAD:P(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
        "O:BAD:P(A;OICI;0x1200a9;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
        // The accounts twice, a refusal, another owner.
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)(A;OICI;0x1200a9;;;BU)",
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(D;OICI;0x1200a9;;;BU)",
        "O:S-1-5-21-1-2-3-1001D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
        "O:BUD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
        // No list, an empty one, no descriptor.
        "O:BAD:NO_ACCESS_CONTROL",
        "O:BAD:P",
        "",
    ] {
        assert!(!is_program_folder_text(text), "{text}");
    }
}

#[test]
fn the_folder_of_a_kept_program_is_made_or_found_and_nothing_else_is_taken_for_it() {
    if !can_make_private() {
        return;
    }
    let sb = Sandbox::new();
    let folder = sb.root.join("CretLink Support");

    prepare_program_folder(&folder).expect("make the folder");
    assert_eq!(Standing::of_dir(&folder), Standing::Ours);
    // A second time it is found as it was made.
    std::fs::write(folder.join("program"), b"x").expect("write");
    prepare_program_folder(&folder).expect("find the folder");
    assert!(folder.join("program").exists());

    // A file under the name is an error, and is left as it is.
    let file = sb.root.join("a file");
    std::fs::write(&file, b"theirs").expect("write");
    let error = prepare_program_folder(&file).expect_err("a file is no folder");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read(&file).expect("read"), b"theirs");
    // The folder above is not made: it must be there, and be the system's.
    assert!(prepare_program_folder(&sb.root.join("missing").join("CretLink Support")).is_err());
    assert!(!sb.root.join("missing").exists());
}

#[cfg(unix)]
#[test]
fn a_link_in_the_place_of_the_folder_of_a_kept_program_is_an_error_and_never_followed() {
    let sb = Sandbox::new();
    let elsewhere = sb.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    let folder = sb.root.join("CretLink Support");
    std::os::unix::fs::symlink(&elsewhere, &folder).expect("link");

    let error = prepare_program_folder(&folder).expect_err("a link is no folder");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        names_in(&elsewhere).is_empty(),
        "nothing was made behind it"
    );
}

#[cfg(not(windows))]
#[test]
fn off_windows_no_program_is_kept() {
    assert_eq!(kept_program_folder(), None);
}

// --- what stands at a name ----------------------------------------------------

#[test]
fn an_entry_is_ours_only_when_it_is_what_it_should_be() {
    let sb = Sandbox::new();
    let dir = sb.root.join("dir");
    let file = sb.root.join("file");
    std::fs::create_dir(&dir).expect("dir");
    std::fs::write(&file, b"x").expect("file");
    let nothing = sb.root.join("nothing");

    assert_eq!(Standing::of_dir(&dir), Standing::Ours);
    assert_eq!(Standing::of_file(&file), Standing::Ours);
    assert_eq!(Standing::of_dir(&nothing), Standing::Absent);
    assert_eq!(Standing::of_file(&nothing), Standing::Absent);
    // A file where a folder belongs, and a folder where a file belongs.
    assert_eq!(Standing::of_dir(&file), Standing::Foreign);
    assert_eq!(Standing::of_file(&dir), Standing::Foreign);
}

#[cfg(unix)]
#[test]
fn a_link_is_never_ours_whatever_it_points_at() {
    let sb = Sandbox::new();
    let dir = sb.root.join("dir");
    let file = sb.root.join("file");
    std::fs::create_dir(&dir).expect("dir");
    std::fs::write(&file, b"x").expect("file");
    let to_dir = sb.root.join("to-dir");
    let to_file = sb.root.join("to-file");
    let to_nothing = sb.root.join("to-nothing");
    std::os::unix::fs::symlink(&dir, &to_dir).expect("link");
    std::os::unix::fs::symlink(&file, &to_file).expect("link");
    std::os::unix::fs::symlink(sb.root.join("nothing"), &to_nothing).expect("link");

    for link in [&to_dir, &to_file, &to_nothing] {
        assert_eq!(Standing::of_dir(link), Standing::Foreign, "{link:?}");
        assert_eq!(Standing::of_file(link), Standing::Foreign, "{link:?}");
    }
}

// --- a folder of the account --------------------------------------------------

#[test]
fn a_folder_of_the_account_is_made_with_the_folders_above_it_and_nothing_is_checked() {
    let sb = Sandbox::new();
    let folder = OwnFolder::same_account(sb.root.join("a").join("b").join("staging"));
    assert!(!folder.is_private());
    assert!(!folder.has_a_data_folder());

    // Nothing is made before the folder is asked for.
    folder.prepare_once().expect("nothing to do");
    assert!(names_in(&sb.root).is_empty());

    folder.prepare().expect("prepare");
    folder.prepare().expect("again");
    assert!(folder.path().is_dir());
    assert!(folder.has_a_data_folder());
}

#[test]
fn a_path_is_a_folder_of_the_account() {
    let path = PathBuf::from("some").join("where");
    let expected = OwnFolder::same_account(path.clone());
    assert_eq!(OwnFolder::from(path.clone()), expected);
    assert_eq!(OwnFolder::from(&path), expected);
    assert_eq!(OwnFolder::from(path.as_path()), expected);
    assert_eq!(OwnFolder::from(&expected), expected);
    assert_eq!(expected.path(), path);
}

// --- a private folder ----------------------------------------------------------

#[test]
fn the_folders_from_the_outermost_private_one_down_are_the_chain() {
    let sb = Sandbox::new();
    assert_eq!(
        sb.staging().chain(),
        vec![sb.data(), sb.data().join("staging")]
    );
    // A folder that is private by itself.
    let alone = OwnFolder::administrators_only(sb.data(), sb.data());
    assert_eq!(alone.chain(), vec![sb.data()]);
    // Given a folder it does not lie in, the folder alone is private: nothing
    // above it is touched.
    let elsewhere = sb.root.join("elsewhere").join("staging");
    let outside = OwnFolder::administrators_only(sb.data(), &elsewhere);
    assert!(outside.is_private());
    assert_eq!(outside.chain(), vec![elsewhere]);
    // A folder of the account has no folders above it that are CretLink's.
    let account = OwnFolder::same_account(sb.root.join("x").join("y"));
    assert_eq!(account.chain(), vec![sb.root.join("x").join("y")]);
}

#[test]
fn every_folder_from_the_data_folder_down_is_prepared() {
    if !can_make_private() {
        return;
    }
    let sb = Sandbox::new();
    let staging = sb.staging();
    assert!(staging.is_private());
    assert!(!staging.has_a_data_folder());

    staging.prepare().expect("prepare");

    assert!(staging.has_a_data_folder());
    assert_eq!(names_in(&sb.root), ["CretLink Data"]);
    assert_eq!(names_in(&sb.data()), ["staging"]);
    // What is there already is kept, and a second folder in the same data
    // folder is made beside the first.
    std::fs::write(staging.path().join("kept"), b"x").expect("write");
    staging.prepare().expect("again");
    let tickets = OwnFolder::administrators_only(sb.data(), sb.data().join("reboot-resume"));
    tickets.prepare().expect("prepare the second");
    assert_eq!(names_in(&sb.data()), ["reboot-resume", "staging"]);
    assert_eq!(names_in(staging.path()), ["kept"]);
}

#[test]
fn a_prepared_folder_is_not_looked_at_again_until_it_is_forgotten() {
    if !can_make_private() {
        return;
    }
    let sb = Sandbox::new();
    let staging = sb.staging();

    staging.prepare_once().expect("the first time prepares");
    assert!(staging.path().is_dir());

    // Gone behind the program's back: the memory still says "prepared".
    std::fs::remove_dir_all(sb.data()).expect("remove");
    staging.prepare_once().expect("remembered");
    assert!(!sb.data().exists());

    staging.forget_prepared();
    staging.prepare_once().expect("prepared again");
    assert!(staging.path().is_dir());
}

#[test]
fn something_else_in_the_place_of_a_private_folder_is_an_error_and_nothing_is_made() {
    if !can_make_private() {
        return;
    }
    // A file where the data folder belongs.
    let sb = Sandbox::new();
    std::fs::write(sb.data(), b"not a folder").expect("write");
    let error = sb.staging().prepare().expect_err("a file is not a folder");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read(sb.data()).expect("read"), b"not a folder");
    assert_eq!(names_in(&sb.root), ["CretLink Data"]);

    // A file where the staging area belongs: the file is left as it is, and
    // the staging area is not usable.
    let sb = Sandbox::new();
    std::fs::create_dir(sb.data()).expect("data");
    std::fs::write(sb.data().join("staging"), b"not a folder").expect("write");
    let error = sb.staging().prepare().expect_err("a file is not a folder");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::read(sb.data().join("staging")).expect("read"),
        b"not a folder"
    );
    assert!(
        sb.staging().prepare_once().is_err(),
        "a failed preparation is not remembered as done"
    );
}

#[cfg(unix)]
#[test]
fn a_link_in_the_place_of_a_private_folder_is_an_error_and_never_followed() {
    let sb = Sandbox::new();
    let elsewhere = sb.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");

    // A link where the data folder belongs.
    std::os::unix::fs::symlink(&elsewhere, sb.data()).expect("link");
    let error = sb.staging().prepare().expect_err("a link is not a folder");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        names_in(&elsewhere).is_empty(),
        "nothing was made behind it"
    );
    assert!(
        std::fs::symlink_metadata(sb.data())
            .expect("still there")
            .file_type()
            .is_symlink(),
        "the link is left as it is"
    );

    // A link where the staging area belongs.
    let sb = Sandbox::new();
    let elsewhere = sb.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    std::fs::create_dir(sb.data()).expect("data");
    std::os::unix::fs::symlink(&elsewhere, sb.data().join("staging")).expect("link");
    assert!(sb.staging().prepare().is_err());
    assert!(names_in(&elsewhere).is_empty());
}

// --- removing --------------------------------------------------------------------

#[test]
fn removing_whole_takes_a_folder_with_everything_in_it_and_a_file() {
    let sb = Sandbox::new();
    let tree = sb.root.join("tree");
    std::fs::create_dir_all(tree.join("a").join("b")).expect("tree");
    std::fs::write(tree.join("a").join("b").join("f"), b"x").expect("file");
    let file = sb.root.join("single");
    std::fs::write(&file, b"x").expect("file");

    remove_whole(&tree).expect("tree");
    remove_whole(&file).expect("file");

    assert!(names_in(&sb.root).is_empty());
    assert_eq!(
        remove_whole(&file).expect_err("gone").kind(),
        std::io::ErrorKind::NotFound
    );
}

#[cfg(unix)]
#[test]
fn removing_whole_takes_a_link_and_never_what_it_points_at() {
    let sb = Sandbox::new();
    let theirs = sb.root.join("theirs");
    std::fs::create_dir(&theirs).expect("theirs");
    std::fs::write(theirs.join("important.txt"), b"theirs").expect("write");

    // A link in the place of what is removed.
    let link = sb.root.join("link");
    std::os::unix::fs::symlink(&theirs, &link).expect("link");
    remove_whole(&link).expect("the link goes");
    assert!(std::fs::symlink_metadata(&link).is_err());

    // Links inside the folder that is removed: to a folder and to a file.
    let tree = sb.root.join("tree");
    std::fs::create_dir_all(tree.join("deep")).expect("tree");
    std::os::unix::fs::symlink(&theirs, tree.join("deep").join("to-folder")).expect("link");
    std::os::unix::fs::symlink(theirs.join("important.txt"), tree.join("to-file")).expect("link");
    remove_whole(&tree).expect("the tree goes");
    assert!(!tree.exists());

    assert_eq!(
        std::fs::read(theirs.join("important.txt")).expect("still there"),
        b"theirs"
    );
}

#[test]
fn empty_folders_are_removed_up_to_the_outermost_and_no_further() {
    if !can_make_private() {
        return;
    }
    // Both folders of a data folder are empty: all of it goes.
    let sb = Sandbox::new();
    let staging = sb.staging();
    let tickets = OwnFolder::administrators_only(sb.data(), sb.data().join("reboot-resume"));
    staging.prepare().expect("prepare");
    tickets.prepare().expect("prepare");
    tickets.remove_if_empty();
    assert_eq!(
        names_in(&sb.data()),
        ["staging"],
        "the data folder still holds the staging area"
    );
    staging.remove_if_empty();
    assert!(
        names_in(&sb.root).is_empty(),
        "nothing is left, and the sandbox above it is not touched"
    );
    // Removing what is not there does nothing.
    staging.remove_if_empty();
    assert!(sb.root.is_dir());

    // A folder that holds something stays, with the folders above it.
    let sb = Sandbox::new();
    let staging = sb.staging();
    staging.prepare().expect("prepare");
    std::fs::write(staging.path().join("kept"), b"x").expect("write");
    staging.remove_if_empty();
    assert_eq!(names_in(staging.path()), ["kept"]);

    // A folder of the account is removed alone: what lies above it is not
    // CretLink's.
    let sb = Sandbox::new();
    let folder = OwnFolder::same_account(sb.root.join("a").join("staging"));
    folder.prepare().expect("prepare");
    folder.remove_if_empty();
    assert_eq!(names_in(&sb.root), ["a"]);
    assert!(names_in(&sb.root.join("a")).is_empty());
}

// --- off Windows -------------------------------------------------------------------

#[cfg(not(windows))]
#[test]
fn off_windows_there_is_one_place_and_no_earlier_release() {
    assert_eq!(earlier_release_folder(), None);
    assert_eq!(administrators_folder(), None);
    assert!(!OwnFolder::staging_for_this_process().is_private());
    assert!(!OwnFolder::tickets_for_this_process().is_private());
    // No program is elevated: the webview keeps its profile where the
    // account's own programs do.
    assert_eq!(OwnFolder::webview_for_this_process(), None);
    // A lock in a folder is never taken for the lock of an earlier release.
    let sb = Sandbox::new();
    let staging = sb.root.join("staging");
    std::fs::create_dir(&staging).expect("staging");
    std::fs::write(staging.join(".in-use.lock"), b"").expect("lock");
    assert!(!earlier_release_in_use(&sb.root));
}

// --- Windows: the owner and the permissions -----------------------------------

#[cfg(windows)]
mod windows {
    use std::path::Path;
    use std::process::Command;

    use super::super::{administrators_own, sys};
    use super::{
        OwnFolder, Sandbox, administrators_folder, can_make_private, earlier_release_folder,
        earlier_release_in_use, is_private_text, is_program_folder_text, kept_program_folder,
        names_in, prepare_program_folder, remove_whole,
    };

    /// The administrators, as `icacls` takes a well-known account.
    const ADMINISTRATORS: &str = "*S-1-5-32-544";

    /// The account that runs the tests, as `icacls` takes it.
    fn this_account() -> String {
        std::env::var("USERNAME").expect("USERNAME")
    }

    fn icacls(path: &Path, arguments: &[&str]) {
        let done = Command::new("icacls")
            .arg(path)
            .args(arguments)
            .output()
            .expect("run icacls");
        assert!(
            done.status.success(),
            "icacls {arguments:?}: {}",
            String::from_utf8_lossy(&done.stdout)
        );
    }

    fn set_owner(path: &Path, owner: &str) {
        icacls(path, &["/setowner", owner]);
    }

    /// Lets everyone write in `path` and in what is made in it, as a folder
    /// under the machine's application data does by default.
    fn open_to_everyone(path: &Path) {
        icacls(path, &["/grant", "*S-1-1-0:(OI)(CI)F"]);
    }

    fn junction(link: &Path, target: &Path) {
        let done = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("run mklink");
        assert!(
            done.status.success(),
            "mklink /J: {}",
            String::from_utf8_lossy(&done.stderr)
        );
    }

    fn permissions(path: &Path) -> String {
        sys::permissions_of(path).expect("read the permissions back")
    }

    /// The folder at `path` belongs to the administrators and has the private
    /// permissions as its own.
    fn assert_private(path: &Path) {
        let text = permissions(path);
        assert!(is_private_text(&text), "{}: {text}", path.display());
    }

    /// Who the permissions in `text` name: the last field of every entry.
    fn named_in(text: &str) -> Vec<String> {
        let (_, access) = text.split_once("D:").expect("an access list");
        access
            .split(')')
            .filter_map(|entry| entry.rsplit(';').next())
            .filter(|who| !who.is_empty() && !who.contains('('))
            .map(str::to_owned)
            .collect()
    }

    /// What is made in a private folder gives rights to nobody but the
    /// administrators and the system.
    fn assert_handed_down(path: &Path) {
        let text = permissions(path);
        let named = named_in(&text);
        assert!(!named.is_empty(), "{text}");
        assert!(
            named.iter().all(|who| who == "SY" || who == "BA"),
            "{}: {text}",
            path.display()
        );
    }

    #[test]
    fn a_new_private_folder_belongs_to_the_administrators_and_is_closed_to_everyone_else() {
        if !can_make_private() {
            return;
        }
        let sb = Sandbox::new();
        // The sandbox itself is open to everyone, as the place of the earlier
        // releases was: nothing of that reaches the folders made in it.
        open_to_everyone(&sb.root);
        let staging = sb.staging();

        staging.prepare().expect("prepare");

        assert_private(&sb.data());
        assert_private(staging.path());
        // What the program then makes in it is closed in the same way.
        let session = staging.path().join("session");
        std::fs::create_dir(&session).expect("create");
        std::fs::write(session.join("manifest.json"), b"{}").expect("write");
        assert_handed_down(&session);
        assert_handed_down(&session.join("manifest.json"));
    }

    /// The folder is made with its owner and its permissions in one step:
    /// nothing else touches it between its creation and this look, in a
    /// folder that is open to everyone.
    #[test]
    fn a_folder_is_private_from_the_moment_it_is_made() {
        if !can_make_private() {
            return;
        }
        let sb = Sandbox::new();
        open_to_everyone(&sb.root);

        sys::create(&sb.data()).expect("create");

        assert_private(&sb.data());
        // A name that is taken is no error, and what stands there is left as
        // it is: the caller looks at it.
        let taken = sb.root.join("taken");
        std::fs::create_dir(&taken).expect("taken");
        let before = permissions(&taken);
        sys::create(&taken).expect("a taken name");
        assert_eq!(permissions(&taken), before);
    }

    /// A folder is never made private afterwards: whoever opened it, or put
    /// something into it, while it was open keeps that.
    #[test]
    fn a_folder_of_the_administrators_with_other_permissions_is_an_error_and_is_left_as_it_is() {
        if !can_make_private() {
            return;
        }
        let sb = Sandbox::new();
        // As an administrator would make it by hand: the permissions of the
        // folder above, and more.
        std::fs::create_dir(sb.data()).expect("data");
        set_owner(&sb.data(), ADMINISTRATORS);
        open_to_everyone(&sb.data());
        let before = permissions(&sb.data());
        assert!(!is_private_text(&before));

        let error = sb
            .staging()
            .prepare()
            .expect_err("not private from its first moment");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(permissions(&sb.data()), before, "nothing was changed");
        assert!(names_in(&sb.data()).is_empty(), "nothing was made in it");
    }

    /// Whose a folder is, is read from its owner through a handle that
    /// follows no link: for `Program Files`, which must be the system's, and
    /// for the folder of an earlier release, which a start with administrator
    /// rights removes unasked only when the administrators own it.
    #[test]
    fn whose_a_folder_is_is_read_from_its_owner_and_never_through_a_link() {
        if !can_make_private() {
            return;
        }
        let sb = Sandbox::new();
        let folder = sb.root.join("folder");
        assert!(!administrators_own(&folder), "nothing is there");
        assert_eq!(sys::trusted_folder(folder.clone()), None);

        std::fs::create_dir(&folder).expect("folder");
        set_owner(&folder, &this_account());
        assert!(!administrators_own(&folder), "an account owns it");
        assert_eq!(sys::trusted_folder(folder.clone()), None);

        set_owner(&folder, ADMINISTRATORS);
        assert!(administrators_own(&folder));
        assert_eq!(sys::trusted_folder(folder.clone()), Some(folder.clone()));

        // A junction to a folder of the administrators is not one of theirs.
        let link = sb.root.join("link");
        junction(&link, &folder);
        assert!(!administrators_own(&link));
        assert_eq!(sys::trusted_folder(link.clone()), None);
        // A file of the administrators is not a folder.
        let file = sb.root.join("file");
        std::fs::write(&file, b"x").expect("file");
        set_owner(&file, ADMINISTRATORS);
        assert!(!administrators_own(&file));
    }

    #[test]
    fn a_folder_of_another_owner_is_an_error_and_is_left_as_it_is() {
        if !can_make_private() {
            return;
        }
        let sb = Sandbox::new();
        std::fs::create_dir(sb.data()).expect("data");
        set_owner(&sb.data(), &this_account());
        let before = permissions(&sb.data());

        let error = sb.staging().prepare().expect_err("not the administrators'");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(permissions(&sb.data()), before, "nothing was changed");
        assert!(names_in(&sb.data()).is_empty(), "nothing was made in it");
    }

    #[test]
    fn a_junction_in_the_place_of_a_private_folder_is_an_error_and_never_followed() {
        if !can_make_private() {
            return;
        }
        let sb = Sandbox::new();
        let elsewhere = sb.root.join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("elsewhere");
        let before = permissions(&elsewhere);

        // A junction where the data folder belongs.
        junction(&sb.data(), &elsewhere);
        let error = sb.staging().prepare().expect_err("a junction is no folder");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            names_in(&elsewhere).is_empty(),
            "nothing was made behind it"
        );
        assert_eq!(permissions(&elsewhere), before, "nothing was changed there");
        std::fs::remove_dir(sb.data()).expect("remove the junction");

        // A junction where the staging area belongs, in a data folder that
        // is private.
        sys::create(&sb.data()).expect("data");
        junction(&sb.data().join("staging"), &elsewhere);
        let error = sb.staging().prepare().expect_err("a junction is no folder");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(names_in(&elsewhere).is_empty());
        assert_eq!(permissions(&elsewhere), before);
    }

    #[test]
    fn removing_whole_takes_a_junction_and_never_what_it_points_at() {
        let sb = Sandbox::new();
        let theirs = sb.root.join("theirs");
        std::fs::create_dir(&theirs).expect("theirs");
        std::fs::write(theirs.join("important.txt"), b"theirs").expect("write");

        // A junction in the place of what is removed.
        let link = sb.root.join("link");
        junction(&link, &theirs);
        remove_whole(&link).expect("the junction goes");
        assert!(std::fs::symlink_metadata(&link).is_err());

        // A junction inside the folder that is removed.
        let tree = sb.root.join("tree");
        std::fs::create_dir_all(tree.join("deep")).expect("tree");
        std::fs::write(tree.join("deep").join("own.txt"), b"own").expect("write");
        junction(&tree.join("deep").join("to-folder"), &theirs);
        remove_whole(&tree).expect("the tree goes");
        assert!(!tree.exists());

        assert_eq!(
            std::fs::read(theirs.join("important.txt")).expect("still there"),
            b"theirs"
        );
    }

    #[test]
    fn a_held_lock_says_that_an_earlier_release_still_runs() {
        let sb = Sandbox::new();
        let earlier = sb.root.join("CretLink");
        assert!(!earlier_release_in_use(&earlier), "nothing is there");
        std::fs::create_dir_all(earlier.join("staging")).expect("staging");
        assert!(!earlier_release_in_use(&earlier), "no lock is there");
        let lock = earlier.join("staging").join(".in-use.lock");
        std::fs::write(&lock, b"").expect("lock");
        assert!(!earlier_release_in_use(&earlier), "nobody holds it");

        // A program of an earlier release holds a share of its lock for as
        // long as it runs.
        let held = std::fs::File::open(&lock).expect("open");
        held.lock_shared().expect("share");
        assert!(earlier_release_in_use(&earlier));
        drop(held);
        assert!(!earlier_release_in_use(&earlier));

        // A folder under the name of the lock is not a lock anyone holds.
        std::fs::remove_file(&lock).expect("remove");
        std::fs::create_dir(&lock).expect("a folder");
        assert!(!earlier_release_in_use(&earlier));
    }

    #[test]
    fn the_places_are_on_the_drive_of_the_windows_folder() {
        let windows = crate::system::windows_folder().expect("the Windows folder");
        let drive = windows.ancestors().last().expect("a drive");

        assert_eq!(
            administrators_folder(),
            Some(drive.join("Program Files").join("CretLink Data"))
        );
        // An elevated program asks the system where the earlier releases kept
        // their files; a program without rights takes it from its
        // environment, as those releases did.
        let machine_data = if sys::elevated() {
            drive.join("ProgramData")
        } else {
            std::env::var_os("ProgramData")
                .map(std::path::PathBuf::from)
                .expect("ProgramData")
        };
        assert_eq!(
            earlier_release_folder(),
            Some(machine_data.join("CretLink"))
        );

        let staging = OwnFolder::staging_for_this_process();
        let tickets = OwnFolder::tickets_for_this_process();
        assert_eq!(staging.is_private(), sys::elevated());
        assert_eq!(tickets.is_private(), sys::elevated());
        if sys::elevated() {
            // Nothing of the environment is in it.
            let data = drive.join("Program Files").join("CretLink Data");
            assert_eq!(staging.path(), data.join("staging"));
            assert_eq!(tickets.path(), data.join("reboot-resume"));
            assert_eq!(staging.chain(), vec![data.clone(), data.join("staging")]);
            // The webview of an elevated window keeps its profiles there too.
            let webview = OwnFolder::webview_for_this_process().expect("a private place");
            assert!(webview.is_private());
            assert_eq!(webview.path(), data.join("webview"));
            assert_eq!(webview.outermost(), data);
        } else {
            assert_eq!(OwnFolder::webview_for_this_process(), None);
            let local = std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA");
            let data = Path::new(&local).join("CretLink");
            assert_eq!(staging.path(), data.join("staging"));
            assert_eq!(tickets.path(), data.join("reboot-resume"));
        }
    }

    // --- the folder of a kept program (ST-M009-017) ---

    /// What the permissions in `text` give the accounts of this computer:
    /// the rights of every entry that names them.
    fn rights_of_the_accounts(text: &str) -> Vec<String> {
        let (_, access) = text.split_once("D:").expect("an access list");
        access
            .split(')')
            .filter_map(|entry| {
                let fields: Vec<&str> = entry.trim_start_matches('(').split(';').collect();
                (fields.len() == 6 && fields[5] == "BU").then(|| fields[2].to_ascii_lowercase())
            })
            .collect()
    }

    /// The folder of a kept program belongs to the administrators; they and
    /// the system can change it, and every account can read it and start a
    /// program in it, and nothing more. What is made in it has the same
    /// permissions, handed down.
    #[test]
    fn the_folder_of_a_kept_program_can_be_changed_by_administrators_only() {
        if !can_make_private() {
            return;
        }
        let sb = Sandbox::new();
        // The sandbox is open to everyone: nothing of that reaches the
        // folder made in it.
        open_to_everyone(&sb.root);
        let folder = sb.root.join("CretLink Support");

        prepare_program_folder(&folder).expect("make the folder");

        let text = permissions(&folder);
        assert!(is_program_folder_text(&text), "{text}");
        assert!(!is_private_text(&text), "{text}");
        // The program that is written into it.
        let program = folder.join("CretLink-Support.exe");
        std::fs::write(&program, b"x").expect("write");
        let text = permissions(&program);
        let named = named_in(&text);
        assert!(
            named
                .iter()
                .all(|who| ["SY", "BA", "BU"].contains(&who.as_str())),
            "{text}"
        );
        assert_eq!(
            rights_of_the_accounts(&text),
            vec!["0x1200a9".to_owned()],
            "the accounts may read and start it, and nothing more: {text}"
        );
        // A second look finds it as it was made.
        prepare_program_folder(&folder).expect("find the folder");
    }

    /// A folder that was not made like this is never put right afterwards:
    /// whoever could write it before may have put something into it.
    #[test]
    fn a_folder_with_other_permissions_is_not_taken_for_the_folder_of_a_kept_program() {
        if !can_make_private() {
            return;
        }
        let sb = Sandbox::new();
        // As an administrator would make it by hand, open to everyone.
        let by_hand = sb.root.join("by hand");
        std::fs::create_dir(&by_hand).expect("create");
        set_owner(&by_hand, ADMINISTRATORS);
        open_to_everyone(&by_hand);
        let before = permissions(&by_hand);

        let error = prepare_program_folder(&by_hand).expect_err("not made by CretLink");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(permissions(&by_hand), before, "nothing was changed");

        // A private folder of CretLink is not one either: no account could
        // start the program in it.
        let private = sb.root.join("private");
        sys::create(&private).expect("create");
        assert!(prepare_program_folder(&private).is_err());

        // A folder of another owner.
        let theirs = sb.root.join("theirs");
        std::fs::create_dir(&theirs).expect("create");
        set_owner(&theirs, &this_account());
        assert!(prepare_program_folder(&theirs).is_err());

        // A junction under the name: nothing is made behind it.
        let elsewhere = sb.root.join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("elsewhere");
        let link = sb.root.join("CretLink Support");
        junction(&link, &elsewhere);
        let error = prepare_program_folder(&link).expect_err("a junction is no folder");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            names_in(&elsewhere).is_empty(),
            "nothing was made behind it"
        );
    }

    #[test]
    fn the_folder_of_a_kept_program_is_in_program_files_on_the_drive_of_windows() {
        let windows = crate::system::windows_folder().expect("the Windows folder");
        let drive = windows.ancestors().last().expect("a drive");
        assert_eq!(
            kept_program_folder(),
            Some(drive.join("Program Files").join("CretLink Support"))
        );
    }

    /// The real place, in continuous verification only: an elevated program
    /// makes its folders in `Program Files`, private from the first moment,
    /// and a recovery that left nothing removes them again.
    #[test]
    fn the_real_place_of_an_elevated_program_is_made_private_and_removed_again() {
        if std::env::var_os("CI").is_none() || !sys::elevated() {
            eprintln!("skipped: not an elevated program in continuous verification");
            return;
        }
        // A folder of its own beside the two of the program, so that tests
        // which use the real staging area at the same moment are not
        // disturbed.
        let data = administrators_folder().expect("the administrators' folder");
        let folder = OwnFolder::administrators_only(&data, data.join("test-of-own-folders"));

        folder.prepare().expect("prepare the real place");

        assert_private(&data);
        assert_private(folder.path());
        std::fs::write(folder.path().join("file"), b"x").expect("write");
        assert_handed_down(&folder.path().join("file"));
        std::fs::remove_file(folder.path().join("file")).expect("remove");
        folder.remove_if_empty();
        assert!(!folder.path().exists());
    }
}

// --- the profile folder of an elevated window (ST-M009-014) -------------------

#[test]
fn the_profile_of_an_elevated_window_is_opened_to_the_account_and_to_nothing_more() {
    let user = "S-1-5-21-1-2-3-1001";
    let text = webview_profile_descriptor(user).expect("a user");
    assert_eq!(
        text,
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)\
         (A;;0x1201bf;;;S-1-5-21-1-2-3-1001)\
         (A;OICIIO;FA;;;S-1-5-21-1-2-3-1001)"
    );
    // On the folder itself the account has no right to remove it, to remove
    // what others made in it, or to change its permissions or its owner.
    let rights = 0x0012_01bf_u32;
    for withheld in [0x0001_0000_u32, 0x0004_0000, 0x0008_0000, 0x0000_0040] {
        assert_eq!(rights & withheld, 0, "{withheld:#x}");
    }
    // Making files and folders in it, and reading it.
    for granted in [
        0x0000_0001_u32,
        0x0000_0002,
        0x0000_0004,
        0x0000_0020,
        0x0002_0000,
    ] {
        assert_eq!(rights & granted, granted, "{granted:#x}");
    }
}

#[test]
fn only_an_identifier_of_an_account_goes_into_the_permissions() {
    for user in [
        "",
        "S-1-",
        "S-1-5-",
        "S-1-5--1",
        "BA",
        "S-1-5-21)(A;OICI;FA;;;WD",
        "S-1-5-21-1 ",
        "s-1-5-18",
    ] {
        assert_eq!(webview_profile_descriptor(user), None, "{user:?}");
    }
    assert!(webview_profile_descriptor(&format!("S-1-5{}", "-4294967295".repeat(20))).is_none());
    assert!(webview_profile_descriptor("S-1-5-18").is_some());
}

#[cfg(windows)]
#[test]
fn a_private_profile_folder_is_opened_to_the_account_of_the_desktop() {
    if !can_make_private() {
        return;
    }
    let sb = Sandbox::new();
    let webview = OwnFolder::administrators_only(sb.data(), sb.data().join("webview"));
    webview.prepare().expect("prepare");
    let profile = webview.path().join("profile");
    std::fs::create_dir(&profile).expect("create");
    std::fs::write(profile.join("cretlink.lock"), b"").expect("lock");
    assert!(super::tests_closed_to_others(&profile));

    open_webview_profile(&profile).expect("open");

    let entries = super::tests_permission_entries(&profile);
    assert!(
        entries.iter().any(|entry| entry == "A;OICI;FA;;;SY"),
        "{entries:?}"
    );
    assert!(
        entries.iter().any(|entry| entry == "A;OICI;FA;;;BA"),
        "{entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.starts_with("A;;0x1201bf;;;S-1-")),
        "{entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.starts_with("A;OICIIO;FA;;;S-1-")),
        "{entries:?}"
    );
    assert_eq!(entries.len(), 4, "{entries:?}");
    // The folder above stays private.
    assert!(super::tests_is_private(webview.path()));
}

#[cfg(unix)]
#[test]
fn off_windows_a_profile_folder_is_left_as_it_is() {
    let sb = Sandbox::new();
    open_webview_profile(&sb.root).expect("nothing to do");
}
