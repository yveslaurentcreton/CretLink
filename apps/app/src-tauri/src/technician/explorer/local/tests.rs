//! Tests of the rules for names and folders on the technician computer
//! (ST-M010-002): what a name from the customer computer may be, the folder
//! of a customer computer, the name of a second copy, and the walk of a
//! dropped folder.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// A folder of this test on the technician computer, empty.
fn scratch(test: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("cretlink-app-local-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the folder");
    dir
}

// --- A name from the customer computer ---

#[test]
fn a_plain_name_is_taken_as_it_is() {
    for name in [
        "report.pdf",
        "Photos",
        "notes (2).txt",
        ".gitignore",
        "a b c.tar.gz",
        "ünïcödé.txt",
        "console.log",
        "COM10.txt",
        "nul-device.txt",
    ] {
        assert_eq!(local_name(name), Ok(name), "{name:?}");
    }
}

/// Nothing a customer computer names leaves the folder: a separator, a
/// drive, a step up, a device, and a name Windows would read as another.
#[test]
fn a_name_that_could_leave_its_folder_is_refused() {
    for (name, why) in [
        ("", "is empty"),
        (".", "stands for a folder, not for an entry"),
        ("..", "stands for a folder, not for an entry"),
        ("..\\..\\Startup\\run.bat", "holds a path separator"),
        ("../../.ssh/authorized_keys", "holds a path separator"),
        ("sub\\file.txt", "holds a path separator"),
        ("/etc/passwd", "holds a path separator"),
        ("C:\\Windows\\system.ini", "holds a path separator"),
        ("C:evil.txt", "holds a colon"),
        ("file.txt:stream", "holds a colon"),
        ("line\nbreak.txt", "holds a control character"),
        ("nul\0.txt", "holds a control character"),
        (
            "what?.txt",
            "holds a character that a file name cannot have",
        ),
        ("a*b", "holds a character that a file name cannot have"),
        ("a|b", "holds a character that a file name cannot have"),
        (
            "quote\".txt",
            "holds a character that a file name cannot have",
        ),
        ("report.pdf.", "ends in a dot or a space"),
        ("report.pdf ", "ends in a dot or a space"),
        ("...", "ends in a dot or a space"),
        ("CON", "is the name of a device"),
        ("con.txt", "is the name of a device"),
        ("NUL", "is the name of a device"),
        ("nul.tar.gz", "is the name of a device"),
        ("Aux.log", "is the name of a device"),
        ("PRN", "is the name of a device"),
        ("COM1", "is the name of a device"),
        ("com9.txt", "is the name of a device"),
        ("LPT1.doc", "is the name of a device"),
        ("COM¹", "is the name of a device"),
        ("CONIN$", "is the name of a device"),
        ("conout$.txt", "is the name of a device"),
        (
            "setup.exe.cretlink-part",
            "is one CretLink keeps for an unfinished download",
        ),
        (
            "SETUP.EXE.CRETLINK-PART",
            "is one CretLink keeps for an unfinished download",
        ),
    ] {
        assert_eq!(local_name(name), Err(why), "{name:?}");
    }
    let long = "a".repeat(256);
    assert_eq!(local_name(&long), Err("is too long"));
    assert!(local_name(&"a".repeat(255)).is_ok());
}

/// The path of a name in a folder lies directly in that folder, or there is
/// no path.
#[test]
fn a_name_in_a_folder_stays_in_that_folder() {
    let folder = Path::new("downloads").join("CUSTOMER-PC");
    assert_eq!(beside(&folder, "report.pdf"), Ok(folder.join("report.pdf")));
    for name in ["..", "..\\x", "../x", "a/b", "C:\\x", "NUL", ""] {
        assert!(beside(&folder, name).is_err(), "{name:?}");
    }

    assert!(stays_inside(&folder, &folder.join("a").join("b.txt")));
    assert!(
        !stays_inside(&folder, &folder),
        "the folder itself is not inside it"
    );
    assert!(!stays_inside(&folder, &folder.join("..").join("x")));
    assert!(!stays_inside(
        &folder,
        Path::new("elsewhere").join("x").as_path()
    ));
}

// --- The folder of a customer computer ---

#[test]
fn the_folder_of_a_customer_computer_is_a_plain_name() {
    assert_eq!(target_folder_name("CUSTOMER-PC"), "CUSTOMER-PC");
    assert_eq!(
        target_folder_name("host-01.example.org"),
        "host-01.example.org"
    );
    assert_eq!(target_folder_name("  ACME-PC-07  "), "ACME-PC-07");
    // What the other computer says its name is cannot lead anywhere.
    assert_eq!(target_folder_name("..\\..\\Startup"), ".._.._Startup");
    assert_eq!(target_folder_name("a/b:c"), "a_b_c");
    for odd in ["", "   ", "..", ".", "///", "NUL", "con", "COM1"] {
        assert_eq!(target_folder_name(odd), "customer-computer", "{odd:?}");
    }
    assert_eq!(target_folder_name("PC."), "PC");
    assert_eq!(target_folder_name(&"x".repeat(200)).chars().count(), 64);

    let downloads = Path::new("home").join("Downloads");
    assert_eq!(
        default_folder(&downloads, "CUSTOMER-PC"),
        downloads.join("CretLink").join("CUSTOMER-PC")
    );
    let odd = default_folder(&downloads, "..\\..\\Windows");
    assert!(stays_inside(&downloads.join("CretLink"), &odd), "{odd:?}");
}

// --- A second copy ---

#[test]
fn a_second_copy_gets_the_next_free_number() {
    let none = |_: &str| false;
    assert_eq!(
        kept_beside("report.pdf", false, &none).as_deref(),
        Some("report (2).pdf")
    );
    assert_eq!(
        kept_beside("archive.tar.gz", false, &none).as_deref(),
        Some("archive.tar (2).gz")
    );
    assert_eq!(
        kept_beside("README", false, &none).as_deref(),
        Some("README (2)")
    );
    assert_eq!(
        kept_beside(".gitignore", false, &none).as_deref(),
        Some(".gitignore (2)")
    );
    // A folder has no extension: its whole name is kept.
    assert_eq!(
        kept_beside("Photos.2026", true, &none).as_deref(),
        Some("Photos.2026 (2)")
    );

    let taken = |name: &str| ["report (2).pdf", "report (3).pdf"].contains(&name);
    assert_eq!(
        kept_beside("report.pdf", false, &taken).as_deref(),
        Some("report (4).pdf")
    );
    assert_eq!(kept_beside("report.pdf", false, &|_| true), None);
}

// --- A dropped folder ---

fn names(paths: &[Vec<String>]) -> Vec<String> {
    paths.iter().map(|path| path.join("\\")).collect()
}

#[test]
fn a_dropped_folder_is_walked_to_its_files_and_folders() {
    let dir = scratch("walk");
    std::fs::create_dir_all(dir.join("sub").join("deeper")).unwrap();
    std::fs::create_dir_all(dir.join("empty")).unwrap();
    std::fs::write(dir.join("a.txt"), b"12345").unwrap();
    std::fs::write(dir.join("sub").join("b.txt"), b"1").unwrap();
    std::fs::write(dir.join("sub").join("deeper").join("c.txt"), b"").unwrap();

    let mut budget = 100;
    let walked = walk(&dir, &mut budget, 32);
    assert_eq!(walked.stopped, None);
    assert!(walked.left_out.is_empty(), "{:?}", walked.left_out);
    // A folder comes before what it holds, and an empty one is there too.
    assert_eq!(names(&walked.folders), ["empty", "sub", "sub\\deeper"]);
    let mut files: Vec<(String, u64)> = walked
        .files
        .iter()
        .map(|(path, len, _)| (path.join("\\"), *len))
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            ("a.txt".to_owned(), 5),
            ("sub\\b.txt".to_owned(), 1),
            ("sub\\deeper\\c.txt".to_owned(), 0),
        ]
    );
    for (path, _, local) in &walked.files {
        let mut expected = dir.clone();
        for name in path {
            expected.push(name);
        }
        assert_eq!(local, &expected);
    }
    assert_eq!(budget, 100 - 6, "three folders and three files were taken");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A walk that reaches a bound says so, and returns no more than the bound.
#[test]
fn a_walk_stops_at_its_bounds_and_says_so() {
    let dir = scratch("walk-bounds");
    for n in 0..20 {
        std::fs::write(dir.join(format!("file-{n:02}.txt")), b"x").unwrap();
    }
    let mut budget = 7;
    let walked = walk(&dir, &mut budget, 32);
    assert_eq!(walked.files.len(), 7);
    assert_eq!(budget, 0);
    assert!(
        walked
            .stopped
            .as_deref()
            .is_some_and(|why| why.contains("more files and folders")),
        "{:?}",
        walked.stopped
    );

    // Deeper than the bound: the folder at the bound is there, what lies
    // below it is not, and the walk says so.
    let deep = scratch("walk-depth");
    std::fs::create_dir_all(deep.join("1").join("2").join("3")).unwrap();
    std::fs::write(deep.join("1").join("2").join("3").join("x.txt"), b"x").unwrap();
    std::fs::write(deep.join("1").join("near.txt"), b"x").unwrap();
    let mut budget = 100;
    let walked = walk(&deep, &mut budget, 2);
    assert_eq!(names(&walked.folders), ["1", "1\\2"]);
    assert_eq!(walked.files.len(), 1, "{:?}", walked.files);
    assert!(
        walked
            .stopped
            .as_deref()
            .is_some_and(|why| why.contains("deeper than 2 levels")),
        "{:?}",
        walked.stopped
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deep);
}

/// A link in a dropped folder is named and not followed: nothing that lies
/// where it points is sent.
#[cfg(unix)]
#[test]
fn a_link_in_a_dropped_folder_is_named_and_not_followed() {
    let dir = scratch("walk-links");
    let outside = scratch("walk-links-outside");
    std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
    std::fs::create_dir_all(dir.join("real")).unwrap();
    std::fs::write(dir.join("real").join("a.txt"), b"a").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("to-folder")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("to-file")).unwrap();
    std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("to-nothing")).unwrap();

    let mut budget = 100;
    let walked = walk(&dir, &mut budget, 32);
    assert_eq!(names(&walked.folders), ["real"]);
    assert_eq!(walked.files.len(), 1);
    assert_eq!(walked.files[0].0, ["real", "a.txt"]);
    let mut left: Vec<&str> = walked
        .left_out
        .iter()
        .map(|out| out.name.as_str())
        .collect();
    left.sort_unstable();
    assert_eq!(left, ["to-file", "to-folder", "to-nothing"]);
    assert!(walked.left_out.iter().all(|out| out.why == LINK_LEFT_OUT));
    assert_eq!(walked.stopped, None);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}
