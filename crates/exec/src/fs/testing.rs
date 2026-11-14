//! What the tests of the file store share (ST-M010-001).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

/// The text under which the file store reports the existing entry `path`. On
/// Windows that is its real location: the long name of every part, in the
/// letter case the disk has, where a test may have spelled the path
/// differently (the temporary folder of a CI runner is spelled with a short
/// name). Elsewhere it is the path as it was given.
pub(super) fn reported(path: &Path) -> String {
    #[cfg(windows)]
    {
        super::anchor::plain(&std::fs::canonicalize(path).expect("the entry exists"))
    }
    #[cfg(not(windows))]
    {
        path.display().to_string()
    }
}

/// Makes `link` a junction to the folder `target`. A junction needs no
/// privilege, unlike a symbolic link.
#[cfg(windows)]
pub(super) fn junction(link: &Path, target: &Path) {
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

/// The verbatim form (`\\?\C:\...`) of the existing folder `path`. A name
/// joined to it is taken by Windows exactly as it is written, so a test can
/// create an entry whose name ends in a dot or a space, or is the name of a
/// device.
#[cfg(windows)]
pub(super) fn verbatim(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).expect("the folder exists")
}

/// The existing folder `path` spelled with its short (8.3) names, or `None`
/// when its volume keeps no short names, in which case the system answers
/// with the long ones.
#[cfg(windows)]
pub(super) fn short_name(path: &Path) -> Option<String> {
    use std::os::windows::process::CommandExt;

    // `%~sI` is the short form of the path the `for` variable holds. The
    // command line is given as it is: `cmd` has its own rules for quotes.
    let asked = std::process::Command::new("cmd")
        .raw_arg(format!(
            "/C for %I in (\"{}\") do @echo %~sI",
            path.display()
        ))
        .output()
        .expect("run cmd");
    assert!(asked.status.success(), "cmd failed");
    let short = String::from_utf8_lossy(&asked.stdout).trim().to_owned();
    let long = path.display().to_string();
    (!short.is_empty() && !short.eq_ignore_ascii_case(&long)).then_some(short)
}
