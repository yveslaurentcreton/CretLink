//! Tests of the webview profile folder's life (ST-M009-014): its creation,
//! its lock, a start that removes a folder that was left and leaves a locked
//! one, a removal that tries again, and what `recover` lists and removes.
//!
//! No test waits for real time: every pause of a [`Patience`] here is zero.

use super::*;

/// A folder of its own for one test, standing in for the temporary folder,
/// removed afterwards.
struct Temp(PathBuf);

impl Temp {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cretlink-webview-profile-{test}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// The path of the profile folder number `number`.
    fn profile(&self, number: u128) -> PathBuf {
        self.0.join(profile_name(number))
    }

    /// A profile folder as a copy that was killed left it: with a lock file
    /// that nobody holds, and what a webview wrote.
    fn stale(&self, number: u128) -> PathBuf {
        let dir = self.profile(number);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(LOCK_FILE), b"").unwrap();
        fill(&dir);
        dir
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// What a webview writes into its profile: nested folders and files.
fn fill(dir: &Path) {
    fs::create_dir_all(dir.join("Cache").join("data")).unwrap();
    fs::write(dir.join("Cache").join("data").join("index"), b"x").unwrap();
    fs::write(dir.join("cookies"), b"x").unwrap();
}

/// Pauses that take no time: three of them.
fn three_tries() -> Patience {
    Patience::of(3, Duration::ZERO)
}

/// A link to the folder `target` at `link`. `false` where this account may
/// not make one (Windows without the right to); continuous verification must
/// be able to.
fn link_to_folder(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(target, link);
    if made.is_err() {
        assert!(
            std::env::var_os("CI").is_none(),
            "continuous verification must be able to make a link: {made:?}"
        );
        eprintln!("skipped: this account may not make a link");
    }
    made.is_ok()
}

// --- Where the profile goes ---

#[test]
fn only_the_installed_technician_copy_keeps_a_profile() {
    let installed = Path::new("/home/ann/.local/share/CretLink/webview");
    for platform in [Platform::Windows, Platform::Linux] {
        // Not installed: the start screen, a technician that runs once.
        assert_eq!(Home::choose(platform, None, false), Home::Temporary);
        // The installed technician copy.
        assert_eq!(
            Home::choose(platform, Some(installed), false),
            Home::Installed(installed.to_path_buf())
        );
        // Get help leaves nothing, wherever it was started from.
        assert_eq!(Home::choose(platform, None, true), Home::Temporary);
        assert_eq!(
            Home::choose(platform, Some(installed), true),
            Home::Temporary
        );
    }
    // WebKit on macOS keeps its data where a program cannot direct it.
    for customer in [false, true] {
        assert_eq!(
            Home::choose(Platform::MacOs, Some(installed), customer),
            Home::Unmanaged
        );
        assert_eq!(
            Home::choose(Platform::MacOs, None, customer),
            Home::Unmanaged
        );
    }
}

#[test]
fn the_folders_lie_in_the_users_data_folder() {
    let dirs = BaseDirs {
        home: PathBuf::from("/home/ann"),
        local_app_data: Some(PathBuf::from("/c/Local")),
        app_data: Some(PathBuf::from("/c/Roaming")),
        data_home: None,
    };
    assert_eq!(
        installed_dir(Platform::Windows, &dirs).as_deref(),
        Some(Path::new("/c/Local/CretLink/webview"))
    );
    assert_eq!(
        legacy_dir(Platform::Windows, &dirs).as_deref(),
        Some(Path::new("/c/Local/dev.cretlink.app"))
    );
    assert_eq!(
        installed_dir(Platform::Linux, &dirs).as_deref(),
        Some(Path::new("/home/ann/.local/share/CretLink/webview"))
    );
    assert_eq!(
        legacy_dir(Platform::Linux, &dirs).as_deref(),
        Some(Path::new("/home/ann/.local/share/dev.cretlink.app"))
    );
    // `$XDG_DATA_HOME` moves both.
    let moved = BaseDirs {
        data_home: Some(PathBuf::from("/data")),
        ..dirs.clone()
    };
    assert_eq!(
        installed_dir(Platform::Linux, &moved).as_deref(),
        Some(Path::new("/data/CretLink/webview"))
    );
    // The webview takes no folder on macOS.
    assert_eq!(installed_dir(Platform::MacOs, &dirs), None);
    assert_eq!(legacy_dir(Platform::MacOs, &dirs), None);
    // Without `%LOCALAPPDATA%` there is no place to name.
    let without = BaseDirs {
        local_app_data: None,
        ..dirs
    };
    assert_eq!(installed_dir(Platform::Windows, &without), None);
}

// --- Creation and the lock ---

#[test]
fn a_new_profile_is_a_new_folder_with_a_generated_name_and_a_held_lock() {
    let temp = Temp::new("create");
    let profile = TempProfile::create_in(temp.path()).unwrap();
    let dir = profile.path().to_path_buf();

    assert_eq!(dir.parent(), Some(temp.path()));
    assert!(is_profile_name(dir.file_name().unwrap()), "{dir:?}");
    assert_eq!(Standing::of_dir(&dir), Standing::Ours);
    assert_eq!(Standing::of_file(&dir.join(LOCK_FILE)), Standing::Ours);
    // Nothing else is in it: the webview comes after the lock.
    assert!(!has_content(&dir).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "for this user only");
    }

    // The lock is held for as long as the profile lives.
    assert!(matches!(probe(&dir), Lock::Held));
    fill(&dir);
    assert!(matches!(probe(&dir), Lock::Held));

    // At the end of the process the folder goes whole.
    profile.remove(&mut Patience::none()).unwrap();
    assert!(is_gone(&dir));
}

#[test]
fn every_profile_has_a_folder_of_its_own() {
    let temp = Temp::new("own");
    let first = TempProfile::create_in(temp.path()).unwrap();
    let second = TempProfile::create_in(temp.path()).unwrap();
    assert_ne!(first.path(), second.path());
    assert_eq!(candidates(temp.path()).unwrap().len(), 2);
}

#[test]
fn a_name_that_is_taken_is_never_taken_over() {
    let temp = Temp::new("taken");
    // A folder that was left under the name, with what it holds.
    let folder = temp.stale(1);
    assert!(TempProfile::create_at(&folder).is_err());
    assert!(folder.join("cookies").exists(), "it is left as it was");
    assert!(folder.join(LOCK_FILE).exists());

    // A file under the name.
    let file = temp.profile(2);
    fs::write(&file, b"theirs").unwrap();
    assert!(TempProfile::create_at(&file).is_err());
    assert_eq!(fs::read(&file).unwrap(), b"theirs");

    // A link under the name: nothing is made behind it.
    let elsewhere = temp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let link = temp.profile(3);
    if link_to_folder(&elsewhere, &link) {
        assert!(TempProfile::create_at(&link).is_err());
        assert!(!elsewhere.join(LOCK_FILE).exists());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn a_temporary_folder_that_is_not_there_gives_no_profile() {
    let temp = Temp::new("missing");
    let missing = temp.path().join("not-there");
    assert!(TempProfile::create_in(&missing).is_err());
    assert!(!missing.exists(), "the temporary folder is not made");
}

#[test]
fn only_generated_names_are_profile_folders() {
    assert!(is_profile_name(OsStr::new(&profile_name(0))));
    assert!(is_profile_name(OsStr::new(&profile_name(u128::MAX))));
    assert_eq!(
        profile_name(0x2a),
        "CretLink-webview-0000000000000000000000000000002a"
    );
    for other in [
        "CretLink-webview-",
        "CretLink-webview-2a",
        "CretLink-webview-0000000000000000000000000000002A",
        "CretLink-webview-0000000000000000000000000000002g",
        "CretLink-webview-0000000000000000000000000000002a0",
        "cretlink-webview-0000000000000000000000000000002a",
        "CretLink-webview-0000000000000000000000000000002a.old",
        "dev.cretlink.app",
    ] {
        assert!(!is_profile_name(OsStr::new(other)), "{other}");
    }
}

/// While the process holds the lock file, nothing removes or renames it: the
/// folder of a running copy never looks like one without a lock.
#[cfg(windows)]
#[test]
fn the_lock_file_of_a_running_copy_cannot_be_removed() {
    let temp = Temp::new("lock-stays");
    let profile = TempProfile::create_in(temp.path()).unwrap();
    let lock = profile.path().join(LOCK_FILE);
    assert!(fs::remove_file(&lock).is_err());
    assert!(fs::rename(&lock, profile.path().join("other")).is_err());
    assert_eq!(Standing::of_file(&lock), Standing::Ours);
    profile.remove(&mut Patience::none()).unwrap();
}

// --- A start that removes what was left ---

#[test]
fn a_start_removes_a_folder_that_was_left_and_leaves_a_locked_one() {
    let temp = Temp::new("sweep");
    let stale = temp.stale(1);
    let running = TempProfile::create_in(temp.path()).unwrap();
    fill(running.path());
    let own = TempProfile::create_in(temp.path()).unwrap();
    // Not a profile folder, although it lies beside them.
    let other = temp.path().join("CretLink-webview-notes");
    fs::create_dir(&other).unwrap();

    let swept = sweep(temp.path(), Some(own.path()), &mut Patience::none()).unwrap();

    assert_eq!(swept.removed, vec![stale.clone()]);
    assert!(
        swept.kept.is_empty() && swept.unknown.is_empty(),
        "{swept:?}"
    );
    assert!(is_gone(&stale));
    // The folder of a copy that runs is never touched.
    assert!(running.path().join("cookies").exists());
    assert!(matches!(probe(running.path()), Lock::Held));
    assert!(own.path().join(LOCK_FILE).exists());
    assert!(other.exists());

    // Once that copy has ended without removing its folder, the next start
    // removes it.
    let left = running.path().to_path_buf();
    drop(running);
    let swept = sweep(temp.path(), Some(own.path()), &mut Patience::none()).unwrap();
    assert_eq!(swept.removed, vec![left.clone()]);
    assert!(is_gone(&left));
}

#[test]
fn the_own_folder_is_skipped_whatever_its_lock_says() {
    let temp = Temp::new("sweep-own");
    // A file system without locks: the own folder's lock looks free.
    let own = temp.stale(1);
    // The same folder, spelled through another path to the temporary folder.
    let spelled = temp.path().join(".").join(own.file_name().unwrap());
    let swept = sweep(temp.path(), Some(&spelled), &mut Patience::none()).unwrap();
    assert_eq!(swept, Sweep::default());
    assert!(own.join("cookies").exists());
}

#[test]
fn a_folder_without_a_lock_file_is_removed_empty_or_left_by_an_unfinished_removal() {
    let temp = Temp::new("sweep-unlocked");
    // Empty: a creation that was given up.
    let empty = temp.profile(1);
    fs::create_dir(&empty).unwrap();
    // With content and no lock file: what an unfinished removal left.
    let half = temp.profile(2);
    fs::create_dir(&half).unwrap();
    fill(&half);

    let swept = sweep(temp.path(), None, &mut Patience::none()).unwrap();

    assert_eq!(swept.removed, vec![empty.clone(), half.clone()]);
    assert!(is_gone(&empty) && is_gone(&half));
}

#[test]
fn a_folder_whose_lock_cannot_be_told_is_left_alone_and_named() {
    let temp = Temp::new("sweep-unknown");
    // A folder stands where the lock file belongs.
    let odd = temp.profile(1);
    fs::create_dir_all(odd.join(LOCK_FILE)).unwrap();
    fill(&odd);

    let swept = sweep(temp.path(), None, &mut Patience::none()).unwrap();

    assert!(
        swept.removed.is_empty() && swept.kept.is_empty(),
        "{swept:?}"
    );
    assert_eq!(swept.unknown.len(), 1);
    assert_eq!(swept.unknown[0].path, odd);
    assert_eq!(
        swept.unknown[0].reason,
        "something that is no file stands where its lock belongs"
    );
    assert!(odd.join("cookies").exists(), "it is left as it was");
}

#[test]
fn a_link_with_a_generated_name_is_not_a_profile_folder() {
    let temp = Temp::new("sweep-link");
    // Somebody else's folder, with a lock file that looks free.
    let theirs = temp.path().join("theirs");
    fs::create_dir(&theirs).unwrap();
    fs::write(theirs.join(LOCK_FILE), b"").unwrap();
    fs::write(theirs.join("keep.txt"), b"theirs").unwrap();
    let link = temp.profile(1);
    if !link_to_folder(&theirs, &link) {
        return;
    }

    assert!(candidates(temp.path()).unwrap().is_empty());
    let swept = sweep(temp.path(), None, &mut Patience::none()).unwrap();
    assert_eq!(swept, Sweep::default());
    assert_eq!(fs::read(theirs.join("keep.txt")).unwrap(), b"theirs");
    assert!(fs::symlink_metadata(&link).is_ok(), "the link is left too");
}

#[test]
fn a_link_inside_a_folder_that_was_left_is_removed_as_a_link() {
    let temp = Temp::new("sweep-link-inside");
    let theirs = temp.path().join("theirs");
    fs::create_dir(&theirs).unwrap();
    fs::write(theirs.join("keep.txt"), b"theirs").unwrap();
    let stale = temp.stale(1);
    if !link_to_folder(&theirs, &stale.join("Cache").join("link")) {
        return;
    }

    let swept = sweep(temp.path(), None, &mut Patience::none()).unwrap();

    assert_eq!(swept.removed, vec![stale.clone()]);
    assert!(is_gone(&stale));
    assert_eq!(fs::read(theirs.join("keep.txt")).unwrap(), b"theirs");
}

#[test]
fn a_temporary_folder_that_is_not_there_holds_nothing() {
    let temp = Temp::new("sweep-missing");
    let missing = temp.path().join("not-there");
    assert_eq!(
        sweep(&missing, None, &mut Patience::none()).unwrap(),
        Sweep::default()
    );
}

// --- A removal that tries again ---

#[test]
fn a_removal_tries_again_while_the_webview_still_holds_files() {
    let temp = Temp::new("retry");
    let dir = temp.stale(1);
    // The webview lets go after two tries.
    let mut tries = 0;
    let mut remove = |path: &Path| {
        tries += 1;
        if tries <= 2 {
            Err(io::Error::other("a file in it is still in use"))
        } else {
            remove_whole(path)
        }
    };
    let mut patience = three_tries();
    remove_with(&dir, &mut patience, &mut remove).unwrap();
    assert_eq!(tries, 3);
    assert!(is_gone(&dir));
    // One pause is left for the next folder of the same operation.
    assert!(patience.pause());
    assert!(!patience.pause());
}

#[test]
fn a_removal_gives_up_when_its_patience_is_over() {
    let temp = Temp::new("retry-over");
    let dir = temp.stale(1);
    let mut tries = 0;
    let mut remove = |_: &Path| {
        tries += 1;
        Err(io::Error::other("a file in it is still in use"))
    };
    let err = remove_with(&dir, &mut three_tries(), &mut remove).unwrap_err();
    assert_eq!(err.to_string(), "a file in it is still in use");
    // The first try and one after each pause.
    assert_eq!(tries, 4);
    assert!(dir.join("cookies").exists());

    // Without patience there is one try.
    tries = 0;
    let mut remove = |_: &Path| {
        tries += 1;
        Err(io::Error::other("held"))
    };
    assert!(remove_with(&dir, &mut Patience::none(), &mut remove).is_err());
    assert_eq!(tries, 1);
}

#[test]
fn a_folder_that_is_gone_is_removed_whoever_removed_it() {
    let temp = Temp::new("retry-gone");
    // Nothing stands there.
    remove_tree(&temp.profile(1), &mut Patience::none()).unwrap();
    // It went while the removal failed.
    let dir = temp.stale(2);
    let mut remove = |path: &Path| {
        remove_whole(path)?;
        Err(io::Error::other("reported too late"))
    };
    remove_with(&dir, &mut Patience::none(), &mut remove).unwrap();
}

#[test]
fn the_installed_profile_goes_and_the_audit_trail_keeps_the_data_folder() {
    let temp = Temp::new("installed");
    let data = temp.path().join("CretLink");
    let profile = data.join(INSTALLED_DIR);
    create_installed(&profile).unwrap();
    // A second start finds it.
    create_installed(&profile).unwrap();
    fill(&profile);
    fs::create_dir(data.join("audit")).unwrap();

    remove_installed(&profile, &mut Patience::none()).unwrap();
    assert!(is_gone(&profile));
    assert!(data.join("audit").is_dir(), "the audit trail stays");

    // Without an audit trail the data folder goes too.
    fs::remove_dir(data.join("audit")).unwrap();
    create_installed(&profile).unwrap();
    remove_installed(&profile, &mut Patience::none()).unwrap();
    assert!(is_gone(&data));
}

// --- What recover lists and removes ---

/// The test's folder as the temporary folder, with the folder of earlier
/// versions in a data folder inside it.
fn places(temp: &Temp) -> Places {
    Places {
        temp: temp.path().to_path_buf(),
        legacy: Some(temp.path().join("data").join(LEGACY_DIR)),
    }
}

#[test]
fn the_places_of_a_user_are_the_temporary_folder_and_the_folder_of_earlier_versions() {
    let dirs = BaseDirs {
        home: PathBuf::from("/home/ann"),
        local_app_data: None,
        app_data: None,
        data_home: None,
    };
    let places = Places::new(Platform::Linux, Some(&dirs), PathBuf::from("/tmp"));
    assert_eq!(places.temp, Path::new("/tmp"));
    assert_eq!(
        places.legacy.as_deref(),
        Some(Path::new("/home/ann/.local/share/dev.cretlink.app"))
    );
    // Without the user's folders only the temporary folder is looked at.
    let places = Places::new(Platform::Linux, None, PathBuf::from("/tmp"));
    assert_eq!(places.legacy, None);
}

#[test]
fn recover_lists_what_was_left_and_removes_nothing() {
    let temp = Temp::new("list");
    let places = places(&temp);
    // Nothing was left.
    assert_eq!(leftovers(&places), Leftovers::default());

    let stale = temp.stale(1);
    let running = TempProfile::create_in(&places.temp).unwrap();
    let odd = temp.profile(2);
    fs::create_dir_all(odd.join(LOCK_FILE)).unwrap();
    let legacy = places.legacy.clone().unwrap();
    fs::create_dir_all(&legacy).unwrap();
    fill(&legacy);

    let listed = leftovers(&places);

    assert_eq!(
        listed.found,
        vec![
            Leftover {
                path: stale.clone(),
                origin: Origin::Stale
            },
            Leftover {
                path: legacy.clone(),
                origin: Origin::EarlierVersion
            },
        ]
    );
    assert_eq!(
        listed.found[0].describe(),
        format!("{} (left by a copy that no longer runs)", stale.display())
    );
    assert_eq!(
        listed.found[1].describe(),
        format!("{} (left by an earlier version)", legacy.display())
    );
    // The folder of a copy that runs is not a leftover; one whose lock
    // cannot be told is named.
    assert_eq!(listed.unknown.len(), 1);
    assert_eq!(listed.unknown[0].path, odd);
    assert_eq!(listed.unreadable, None);
    // Nothing was removed, and the lock that was tried is free again.
    assert!(stale.join("cookies").exists() && legacy.join("cookies").exists());
    assert!(matches!(probe(&stale), Lock::Free(_)));
    assert!(matches!(probe(running.path()), Lock::Held));
}

#[test]
fn recover_removes_what_was_left_and_says_what_it_kept() {
    let temp = Temp::new("recover");
    let places = places(&temp);
    let stale = temp.stale(1);
    let running = TempProfile::create_in(&places.temp).unwrap();
    fill(running.path());
    let odd = temp.profile(2);
    fs::create_dir_all(odd.join(LOCK_FILE)).unwrap();
    let legacy = places.legacy.clone().unwrap();
    fs::create_dir_all(&legacy).unwrap();
    fill(&legacy);

    let recovery = recover(&places, &mut Patience::none());

    let stale_leftover = Leftover {
        path: stale.clone(),
        origin: Origin::Stale,
    };
    let legacy_leftover = Leftover {
        path: legacy.clone(),
        origin: Origin::EarlierVersion,
    };
    assert_eq!(
        recovery.found,
        vec![stale_leftover.clone(), legacy_leftover.clone()]
    );
    assert_eq!(recovery.removed, vec![stale_leftover, legacy_leftover]);
    assert!(recovery.kept.is_empty(), "{recovery:?}");
    assert!(is_gone(&stale) && is_gone(&legacy));
    // The folder beside the one of earlier versions is not CretLink's.
    assert!(legacy.parent().unwrap().is_dir());
    // A copy that runs keeps its folder, and what cannot be told is named
    // and left.
    assert!(running.path().join("cookies").exists());
    assert_eq!(recovery.unknown.len(), 1);
    assert!(odd.join(LOCK_FILE).is_dir());

    // A second run finds nothing to remove.
    let again = recover(&places, &mut Patience::none());
    assert!(again.found.is_empty() && again.removed.is_empty());
}

#[test]
fn a_link_under_the_name_of_earlier_versions_is_removed_as_a_link() {
    let temp = Temp::new("recover-link");
    let places = places(&temp);
    let theirs = temp.path().join("theirs");
    fs::create_dir(&theirs).unwrap();
    fs::write(theirs.join("keep.txt"), b"theirs").unwrap();
    let legacy = places.legacy.clone().unwrap();
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    if !link_to_folder(&theirs, &legacy) {
        return;
    }

    let recovery = recover(&places, &mut Patience::none());

    assert_eq!(recovery.removed.len(), 1, "{recovery:?}");
    assert!(is_gone(&legacy));
    assert_eq!(fs::read(theirs.join("keep.txt")).unwrap(), b"theirs");
}

#[cfg(unix)]
#[test]
fn a_temporary_folder_that_cannot_be_listed_is_unknown_not_empty() {
    use std::os::unix::fs::PermissionsExt;

    let temp = Temp::new("unreadable");
    let places = Places {
        temp: temp.path().join("closed"),
        legacy: None,
    };
    fs::create_dir(&places.temp).unwrap();
    fs::set_permissions(&places.temp, fs::Permissions::from_mode(0o000)).unwrap();
    let listed = leftovers(&places);
    let recovery = recover(&places, &mut Patience::none());
    fs::set_permissions(&places.temp, fs::Permissions::from_mode(0o700)).unwrap();

    // An account that the permissions do not hold for (root) can list it.
    if let Some(reason) = listed.unreadable {
        assert_eq!(
            reason,
            format!(
                "the temporary folder {} could not be listed (permission denied)",
                places.temp.display()
            )
        );
        assert!(recovery.unreadable.is_some());
        assert!(recovery.found.is_empty());
    }
}
