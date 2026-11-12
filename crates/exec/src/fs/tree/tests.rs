//! Tests of the folder removal (ST-M010-001) against a real, sandboxed file
//! system: every test works in a unique temporary directory that it removes
//! when it ends.
//!
//! The walk, its bounds, its honest states and the handling of links are
//! covered on every platform. What only Windows has — junctions, a file held
//! open by another program, the read-only attribute — is covered by the tests
//! marked `cfg(windows)`, and the permission-based refusals by the ones marked
//! `cfg(unix)`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cretlink_protocol::{ErrorCode, Message, RequestId, SessionId, TreeState};

use super::{StepBounds, StepReport, is_drive, remove_step};

/// A unique temporary directory, removed when the guard drops.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("cretlink-tree-test-{pid}-{n}"));
        std::fs::create_dir_all(&root).expect("create the sandbox");
        Self { root }
    }

    /// A path in the sandbox, from its parts separated by `/`. The parts are
    /// joined one by one, so the path has the separators of the platform
    /// throughout and compares equal, as text, with the paths the walk builds.
    fn path(&self, relative: &str) -> PathBuf {
        relative
            .split('/')
            .fold(self.root.clone(), |path, part| path.join(part))
    }

    /// Creates the folder `relative` (and those above it) and returns it.
    fn dir(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(&path).expect("create a folder");
        path
    }

    /// Creates the file `relative` (and the folders above it) and returns it.
    fn file(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the folder of a file");
        }
        std::fs::write(&path, relative.as_bytes()).expect("write a file");
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // A test may have taken write access away from a folder.
        #[cfg(unix)]
        make_writable(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Gives every folder under `path` its write access back, so the sandbox can
/// be removed.
#[cfg(unix)]
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

/// Bounds no test folder reaches.
fn roomy() -> StepBounds {
    StepBounds {
        entries: 100_000,
        time: Duration::from_secs(600),
        depth: 64,
    }
}

/// A removal nobody stops.
fn never() -> bool {
    false
}

/// One step with roomy bounds that nobody stops.
#[track_caller]
fn remove(root: &Path) -> StepReport {
    let report = remove_step(root, roomy(), &never).expect("the folder is taken up");
    assert_on_the_wire(&report);
    report
}

/// Every answer of a step must be one the wire carries: within its bounds,
/// every text on one line, and small enough for one record of the secure
/// channel.
#[track_caller]
fn assert_on_the_wire(report: &StepReport) {
    let message = Message::Response {
        id: RequestId(1),
        session: SessionId::new(),
        result: report.clone().into_result(),
    };
    let encoded = message
        .encode()
        .expect("the answer of a step is within the wire bounds");
    assert!(encoded.len() < 60_000, "{} bytes", encoded.len());
}

/// The paths a report names as not removed, sorted.
fn failed_paths(report: &StepReport) -> Vec<String> {
    let mut paths: Vec<String> = report.failed.iter().map(|f| f.path.clone()).collect();
    paths.sort();
    paths
}

/// How many entries a folder holds, at any depth.
fn count_entries(path: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            count += 1;
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir {
                count += count_entries(&entry.path());
            }
        }
    }
    count
}

// --- the whole folder -----------------------------------------------------

#[test]
fn a_folder_with_everything_in_it_is_removed() {
    let sb = Sandbox::new();
    let root = sb.dir("old");
    sb.file("old/a.txt");
    sb.file("old/b.bin");
    sb.dir("old/empty");
    sb.file("old/sub/c.txt");
    sb.file("old/sub/deeper/d.txt");
    // What lies beside the folder is not its content.
    let beside = sb.file("beside.txt");

    let report = remove(&root);

    assert_eq!(report.state, TreeState::Removed);
    assert_eq!(report.removed_files, 4, "a.txt, b.bin, c.txt and d.txt");
    assert_eq!(report.removed_dirs, 4, "empty, deeper, sub and old itself");
    assert!(report.failed.is_empty());
    assert_eq!(report.failed_more, 0);
    assert!(!root.exists(), "the folder is gone");
    assert!(beside.exists(), "what lies beside the folder is untouched");
}

#[test]
fn an_empty_folder_is_removed() {
    let sb = Sandbox::new();
    let root = sb.dir("empty");

    let report = remove(&root);

    assert_eq!(report.state, TreeState::Removed);
    assert_eq!((report.removed_files, report.removed_dirs), (0, 1));
    assert!(!root.exists());
}

#[test]
fn a_folder_that_is_not_there_is_not_found() {
    let sb = Sandbox::new();
    let error = remove_step(&sb.path("nothing-here"), roomy(), &never)
        .expect_err("a missing folder cannot be removed");
    assert_eq!(error.code, ErrorCode::PathNotFound);
}

#[test]
fn a_file_is_not_removed_as_a_folder() {
    let sb = Sandbox::new();
    let file = sb.file("report.txt");
    let error = remove_step(&file, roomy(), &never).expect_err("a file is not a folder to remove");
    assert_eq!(error.code, ErrorCode::WrongPathKind);
    assert!(file.exists(), "the file is untouched");
}

/// A drive is never removed "as a folder". The refusal is decided from the
/// path alone, before anything on the disk is looked at, so the test can name
/// real drives without any risk to them.
#[test]
fn a_drive_is_not_a_folder() {
    #[cfg(unix)]
    assert!(is_drive(Path::new("/")));
    #[cfg(windows)]
    for drive in ["C:\\", "c:/", "D:\\"] {
        assert!(is_drive(Path::new(drive)), "{drive}");
    }
    assert!(!is_drive(&std::env::temp_dir().join("a-folder")));

    // The step itself refuses, shown with a path that has no folder above it
    // and names nothing on the disk.
    let error = remove_step(Path::new(""), roomy(), &never)
        .expect_err("a path without a folder above it is refused");
    assert_eq!(error.code, ErrorCode::InvalidPath);
}

// --- bounds ---------------------------------------------------------------

/// A step settles no more entries than its bound and says that more remain;
/// the following steps get further, and the counts of the steps add up to the
/// whole folder.
#[test]
fn a_step_stops_at_its_entry_bound_and_the_next_one_gets_further() {
    let sb = Sandbox::new();
    let root = sb.dir("big");
    for n in 0..20 {
        sb.file(&format!("big/file-{n:02}.txt"));
    }
    for n in 0..5 {
        sb.file(&format!("big/sub/inner-{n}.txt"));
    }
    let bounds = StepBounds {
        entries: 10,
        ..roomy()
    };

    let (mut files, mut dirs, mut steps) = (0, 0, 0);
    loop {
        let report = remove_step(&root, bounds, &never).expect("a step");
        assert_on_the_wire(&report);
        steps += 1;
        assert!(
            report.removed_files + report.removed_dirs <= bounds.entries,
            "a step settles at most its bound: {report:?}"
        );
        files += report.removed_files;
        dirs += report.removed_dirs;
        match report.state {
            TreeState::More => {
                assert!(root.exists(), "an unfinished removal leaves the folder");
                assert_eq!(report.removed_files + report.removed_dirs, bounds.entries);
            }
            TreeState::Removed => break,
            other => panic!("unexpected state {other:?}"),
        }
        assert!(steps < 100, "the removal must end");
    }
    assert_eq!(files, 25);
    assert_eq!(dirs, 2, "sub and big itself");
    assert_eq!(steps, 3, "27 entries in steps of ten");
    assert!(!root.exists());
}

/// Once a step has removed something it answers when its time is up, however
/// much is left; with no time at all that is after one entry.
#[test]
fn a_step_answers_when_its_time_is_up() {
    let sb = Sandbox::new();
    let root = sb.dir("slow");
    for n in 0..4 {
        sb.file(&format!("slow/{n}.txt"));
    }
    let bounds = StepBounds {
        time: Duration::ZERO,
        ..roomy()
    };

    let first = remove_step(&root, bounds, &never).expect("a step");
    assert_eq!(first.state, TreeState::More);
    assert_eq!(first.removed_files + first.removed_dirs, 1);
    assert_eq!(count_entries(&root), 3);

    let mut steps = 1;
    while remove_step(&root, bounds, &never).expect("a step").state == TreeState::More {
        steps += 1;
        assert!(steps < 100, "the removal must end");
    }
    assert!(!root.exists());
}

/// A folder nested deeper than the walk goes is named and left, with
/// everything above it; what the walk can reach beside it is removed.
#[test]
fn a_folder_nested_deeper_than_the_bound_is_left() {
    let sb = Sandbox::new();
    let root = sb.dir("nest");
    sb.file("nest/beside.txt");
    sb.file("nest/1/2/3/4/bottom.txt");
    let bounds = StepBounds {
        depth: 2,
        ..roomy()
    };

    let report = remove_step(&root, bounds, &never).expect("a step");
    assert_on_the_wire(&report);

    assert_eq!(report.state, TreeState::Incomplete);
    assert_eq!(report.removed_files, 1, "beside.txt");
    assert_eq!(report.removed_dirs, 0, "every folder holds what was left");
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].code, ErrorCode::LimitExceeded);
    assert!(
        Path::new(&report.failed[0].path).ends_with(Path::new("nest/1/2/3")),
        "{}",
        report.failed[0].path
    );
    assert!(sb.path("nest/1/2/3/4/bottom.txt").exists());
}

// --- stop -----------------------------------------------------------------

#[test]
fn a_step_that_is_stopped_at_once_removes_nothing() {
    let sb = Sandbox::new();
    let root = sb.dir("keep");
    sb.file("keep/a.txt");

    let report = remove_step(&root, roomy(), &|| true).expect("a step");

    assert_eq!(report.state, TreeState::Stopped);
    assert_eq!((report.removed_files, report.removed_dirs), (0, 0));
    assert!(sb.path("keep/a.txt").exists());
}

/// The step asks before every entry whether to stop. Stopped in the middle, it
/// ends there and reports exactly what it had removed until then.
#[test]
fn a_step_that_is_stopped_midway_reports_what_it_had_removed() {
    let sb = Sandbox::new();
    let root = sb.dir("half");
    for n in 0..10 {
        sb.file(&format!("half/{n}.txt"));
    }
    let asked = Cell::new(0u32);
    let stop_at_the_fourth = || {
        asked.set(asked.get() + 1);
        asked.get() >= 4
    };

    let report = remove_step(&root, roomy(), &stop_at_the_fourth).expect("a step");
    assert_on_the_wire(&report);

    assert_eq!(report.state, TreeState::Stopped);
    assert_eq!(
        report.removed_files, 3,
        "one entry for each time it went on"
    );
    assert_eq!(report.removed_dirs, 0);
    assert_eq!(count_entries(&root), 7, "the rest is still there");
}

// --- links ----------------------------------------------------------------

/// A name in the folder that is one more name of a file elsewhere (a hard
/// link) is removed as a name: the file stays under its other name.
#[test]
fn a_second_name_of_a_file_elsewhere_is_removed_and_the_file_stays() {
    let sb = Sandbox::new();
    let kept = sb.file("elsewhere/data.bin");
    let root = sb.dir("old");
    std::fs::hard_link(&kept, sb.path("old/alias.bin")).expect("give the file a second name");

    let report = remove(&root);

    assert_eq!(report.state, TreeState::Removed, "{report:?}");
    assert_eq!(report.removed_files, 1);
    assert!(!root.exists());
    assert_eq!(
        std::fs::read(&kept).expect("the file is still there"),
        b"elsewhere/data.bin"
    );
}

/// Links in the folder are removed as links. What they point at — a folder
/// and a file elsewhere, the folder itself, nothing at all — is never walked
/// and never removed.
#[cfg(unix)]
#[test]
fn links_in_the_folder_are_removed_and_never_followed() {
    use std::os::unix::fs::symlink;

    let sb = Sandbox::new();
    let outside_dir = sb.dir("elsewhere/folder");
    let outside_inner = sb.file("elsewhere/folder/inner.txt");
    let outside_file = sb.file("elsewhere/keep.txt");
    let root = sb.dir("old");
    sb.file("old/own.txt");
    sb.dir("old/sub");
    symlink(&outside_dir, sb.path("old/to-folder")).expect("link to a folder elsewhere");
    symlink(&outside_file, sb.path("old/to-file")).expect("link to a file elsewhere");
    symlink(&root, sb.path("old/sub/to-itself")).expect("link back to the folder");
    symlink(sb.path("nowhere"), sb.path("old/dangling")).expect("link to nothing");

    let report = remove(&root);

    assert_eq!(report.state, TreeState::Removed, "{report:?}");
    assert_eq!(report.removed_files, 5, "own.txt and the four links");
    assert_eq!(report.removed_dirs, 2, "sub and old itself");
    assert!(!root.exists());
    assert!(outside_dir.is_dir(), "the linked folder is untouched");
    assert!(outside_inner.exists(), "and so is what it holds");
    assert!(outside_file.exists(), "the linked file is untouched");
}

/// A link given as the folder to remove is removed as a link: the folder it
/// points at keeps everything.
#[cfg(unix)]
#[test]
fn a_link_in_the_place_of_the_folder_is_removed_as_a_link() {
    let sb = Sandbox::new();
    let real = sb.dir("real");
    let inner = sb.file("real/inner.txt");
    let link = sb.path("link");
    std::os::unix::fs::symlink(&real, &link).expect("link to the folder");

    let report = remove(&link);

    assert_eq!(report.state, TreeState::Removed);
    assert_eq!((report.removed_files, report.removed_dirs), (1, 0));
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the link is gone"
    );
    assert!(real.is_dir());
    assert!(inner.exists());
}

// --- what cannot be removed -------------------------------------------------

/// Takes the right to remove entries away from the folder `path`. Returns
/// `false` when that does not hold the test's own account back (it runs as
/// the superuser), in which case the caller has nothing to test.
#[cfg(unix)]
fn seal(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555))
        .expect("take write access away");
    let probe = path.join(".probe");
    if std::fs::write(&probe, b"").is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipped: this account is not held back by permissions");
        return false;
    }
    true
}

/// What cannot be removed is named with its reason, keeps the folders above
/// it, and stops nothing else: the rest of the folder is removed. The result
/// is not a success.
#[cfg(unix)]
#[test]
fn what_cannot_be_removed_is_named_and_the_rest_is_removed() {
    let sb = Sandbox::new();
    let root = sb.dir("old");
    sb.file("old/free.txt");
    sb.file("old/also-free/x.txt");
    let a = sb.file("old/sealed/a.txt");
    let b = sb.file("old/sealed/b.txt");
    if !seal(&sb.path("old/sealed")) {
        return;
    }

    let report = remove(&root);

    assert_eq!(report.state, TreeState::Incomplete, "never a success");
    assert_eq!(report.removed_files, 2, "free.txt and x.txt");
    assert_eq!(report.removed_dirs, 1, "also-free");
    assert_eq!(report.failed_more, 0);
    assert_eq!(
        failed_paths(&report),
        vec![a.display().to_string(), b.display().to_string()],
        "the two entries that could not be removed, not the folders above them"
    );
    for failure in &report.failed {
        assert_eq!(failure.code, ErrorCode::AccessDenied);
    }
    assert!(a.exists() && b.exists(), "they are still there");
    assert!(root.exists(), "and so are the folders that hold them");
    assert!(!sb.path("old/free.txt").exists());
    assert!(!sb.path("old/also-free").exists());

    // A second step meets the same entries again and says the same.
    let again = remove(&root);
    assert_eq!(again.state, TreeState::Incomplete);
    assert_eq!((again.removed_files, again.removed_dirs), (0, 0));
    assert_eq!(failed_paths(&again), failed_paths(&report));
}

/// The answer names the first entries it could not remove and counts the rest.
#[cfg(unix)]
#[test]
fn failures_beyond_the_ones_named_are_counted() {
    let sb = Sandbox::new();
    let root = sb.dir("old");
    let named = cretlink_protocol::limits::MAX_TREE_FAILURES;
    let extra = 5;
    for n in 0..named + extra {
        sb.file(&format!("old/sealed/{n:02}.txt"));
    }
    if !seal(&sb.path("old/sealed")) {
        return;
    }

    let report = remove(&root);

    assert_eq!(report.state, TreeState::Incomplete);
    assert_eq!(report.failed.len(), named);
    assert_eq!(report.failed_more, extra as u64);
    assert_eq!((report.removed_files, report.removed_dirs), (0, 0));
}

/// A step that reaches its bound without having removed anything says that it
/// is blocked, not that more remains: another step would meet the same
/// entries again.
#[cfg(unix)]
#[test]
fn a_step_that_can_remove_nothing_is_blocked() {
    let sb = Sandbox::new();
    let root = sb.dir("old");
    for n in 0..6 {
        sb.file(&format!("old/sealed/{n}.txt"));
    }
    if !seal(&sb.path("old/sealed")) {
        return;
    }
    let bounds = StepBounds {
        entries: 4,
        ..roomy()
    };

    let report = remove_step(&root, bounds, &never).expect("a step");
    assert_on_the_wire(&report);

    assert_eq!(report.state, TreeState::Blocked);
    assert_eq!((report.removed_files, report.removed_dirs), (0, 0));
    assert_eq!(report.failed.len() as u64 + report.failed_more, 4);
    assert_eq!(count_entries(&root), 7, "sealed and its six files");
}

/// A name is the target's own text. One that holds a line break is reported
/// on one line, so it cannot break a list or a log into forged lines.
#[cfg(unix)]
#[test]
fn a_name_with_a_line_break_is_reported_on_one_line() {
    let sb = Sandbox::new();
    let root = sb.dir("old");
    sb.file("old/sealed/first\nsecond.txt");
    if !seal(&sb.path("old/sealed")) {
        return;
    }

    let report = remove(&root);

    assert_eq!(report.failed.len(), 1);
    let path = &report.failed[0].path;
    assert!(!path.contains('\n'), "{path:?}");
    assert!(
        path.ends_with("first\u{FFFD}second.txt"),
        "the break is shown as a replaced character: {path:?}"
    );
}

// --- Windows only ----------------------------------------------------------

#[cfg(windows)]
mod windows {
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::Path;

    use cretlink_protocol::{ErrorCode, TreeState};

    use super::{Sandbox, failed_paths, remove};

    /// Makes `link` a junction to the folder `target`. A junction needs no
    /// privilege, unlike a symbolic link.
    fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .expect("run mklink");
        assert!(
            made.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );
    }

    /// A junction in the folder is removed as a link: the folder it points at,
    /// elsewhere on the disk, keeps everything.
    #[test]
    fn a_junction_in_the_folder_is_removed_and_never_followed() {
        let sb = Sandbox::new();
        let outside = sb.dir("elsewhere/folder");
        let inner = sb.file("elsewhere/folder/inner.txt");
        let root = sb.dir("old");
        sb.file("old/own.txt");
        junction(&sb.path("old/to-folder"), &outside);
        junction(&sb.path("old/sub-junction-to-itself"), &root);

        let report = remove(&root);

        assert_eq!(report.state, TreeState::Removed, "{report:?}");
        assert_eq!(report.removed_files, 3, "own.txt and the two junctions");
        assert_eq!(report.removed_dirs, 1, "old itself");
        assert!(!root.exists());
        assert!(outside.is_dir(), "the linked folder is untouched");
        assert!(inner.exists(), "and so is what it holds");
    }

    /// A junction given as the folder to remove is removed as a link.
    #[test]
    fn a_junction_in_the_place_of_the_folder_is_removed_as_a_link() {
        let sb = Sandbox::new();
        let real = sb.dir("real");
        let inner = sb.file("real/inner.txt");
        let link = sb.path("link");
        junction(&link, &real);

        let report = remove(&link);

        assert_eq!(report.state, TreeState::Removed, "{report:?}");
        assert_eq!((report.removed_files, report.removed_dirs), (1, 0));
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "the link is gone"
        );
        assert!(real.is_dir());
        assert!(inner.exists());
    }

    /// Symbolic links to a folder and to a file are removed as links. Creating
    /// one needs a privilege that an unelevated account may lack; the test
    /// then has nothing to test.
    #[test]
    fn symbolic_links_in_the_folder_are_removed_and_never_followed() {
        use std::os::windows::fs::{symlink_dir, symlink_file};

        let sb = Sandbox::new();
        let outside_dir = sb.dir("elsewhere/folder");
        let outside_inner = sb.file("elsewhere/folder/inner.txt");
        let outside_file = sb.file("elsewhere/keep.txt");
        let root = sb.dir("old");
        if let Err(error) = symlink_dir(&outside_dir, sb.path("old/to-folder")) {
            eprintln!("skipped: this account may not create symbolic links ({error})");
            return;
        }
        symlink_file(&outside_file, sb.path("old/to-file")).expect("link to a file elsewhere");

        let report = remove(&root);

        assert_eq!(report.state, TreeState::Removed, "{report:?}");
        assert_eq!(report.removed_files, 2, "the two links");
        assert!(!root.exists());
        assert!(outside_dir.is_dir());
        assert!(outside_inner.exists());
        assert!(outside_file.exists());
    }

    /// A file that another program holds open is named as in use and keeps the
    /// folder; the rest is removed. Once the file is released, the next step
    /// removes it and the folder.
    #[test]
    fn a_file_held_open_by_another_program_is_named_as_in_use() {
        // FILE_SHARE_READ only: no other handle may delete the file while
        // this one is open.
        const FILE_SHARE_READ: u32 = 0x0000_0001;

        let sb = Sandbox::new();
        let root = sb.dir("old");
        sb.file("old/free.txt");
        let held = sb.file("old/sub/held.bin");
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&held)
            .expect("hold the file open");

        let report = remove(&root);

        assert_eq!(report.state, TreeState::Incomplete, "never a success");
        assert_eq!(report.removed_files, 1, "free.txt");
        assert_eq!(report.removed_dirs, 0);
        assert_eq!(failed_paths(&report), vec![held.display().to_string()]);
        assert_eq!(report.failed[0].code, ErrorCode::FileInUse);
        assert!(held.exists());
        assert!(root.exists());

        drop(handle);
        let report = remove(&root);
        assert_eq!(report.state, TreeState::Removed, "{report:?}");
        assert_eq!(report.removed_files, 1);
        assert_eq!(report.removed_dirs, 2, "sub and old");
        assert!(!root.exists());
    }

    /// A read-only file and a folder marked read-only are removed like any
    /// other: the attribute protects against changes, not against the removal
    /// of the folder they are in.
    #[test]
    fn read_only_entries_are_removed() {
        let sb = Sandbox::new();
        let root = sb.dir("old");
        let file = sb.file("old/sub/fixed.txt");
        for path in [&file, &sb.path("old/sub")] {
            let mut permissions = std::fs::metadata(path).expect("stat").permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(path, permissions).expect("mark read-only");
        }

        let report = remove(&root);

        assert_eq!(report.state, TreeState::Removed, "{report:?}");
        assert_eq!((report.removed_files, report.removed_dirs), (1, 2));
        assert!(!root.exists());
    }

    /// A folder whose paths are longer than the classic 260 characters is
    /// removed like any other.
    #[test]
    fn a_folder_with_very_long_paths_is_removed() {
        let sb = Sandbox::new();
        let root = sb.dir("old");
        let mut deep = String::from("old");
        for _ in 0..8 {
            deep.push('/');
            deep.push_str(&"n".repeat(40));
        }
        sb.file(&format!("{deep}/bottom.txt"));

        let report = remove(&root);

        assert_eq!(report.state, TreeState::Removed, "{report:?}");
        assert_eq!((report.removed_files, report.removed_dirs), (1, 9));
        assert!(!root.exists());
    }
}
