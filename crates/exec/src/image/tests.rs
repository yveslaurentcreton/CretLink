//! Tests of the held program file (ST-M009-017).
//!
//! The file of a running program can be renamed, so the test that moves a
//! program away and puts another file under its path runs on a copy of this
//! test program, started from a folder of its own: the program that the build
//! made is never touched.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::hold_running_program;

use super::hold_checked;

/// A file that is held for a test, and says when it was given back.
struct Held<'a>(&'a std::cell::Cell<bool>);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

/// The image was moved between the two looks: what was held under the first
/// name is given back, and the caller is told.
#[test]
fn a_program_that_moved_between_the_two_looks_is_not_held() {
    let given_back = std::cell::Cell::new(false);
    let mut names = ["first", "second"].into_iter();
    let held = hold_checked(
        || Ok(names.next().unwrap_or("none")),
        |name| {
            assert_eq!(*name, "first", "the file under the first name is held");
            Ok(Held(&given_back))
        },
        |_| Ok(true),
    );
    let error = held.err().expect("nothing is held");
    assert_eq!(error.kind(), std::io::ErrorKind::ResourceBusy);
    assert!(given_back.get(), "what was held is given back");
}

/// The image lay under the same name before and after: what was held is the
/// image, and it stays held.
#[test]
fn a_program_that_stayed_where_it_lies_is_held() {
    let given_back = std::cell::Cell::new(false);
    let held =
        hold_checked(|| Ok("same"), |_| Ok(Held(&given_back)), |_| Ok(true)).expect("it is held");
    assert!(!given_back.get());
    drop(held);
    assert!(given_back.get());
}

/// What stands under the name is no file: nothing is held.
#[test]
fn what_is_no_file_is_not_held() {
    let given_back = std::cell::Cell::new(false);
    let held = hold_checked(|| Ok("same"), |_| Ok(Held(&given_back)), |_| Ok(false));
    let error = held.err().expect("nothing is held");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(given_back.get());
}

#[cfg(not(windows))]
#[test]
fn off_windows_no_program_is_held() {
    let error = hold_running_program().expect_err("nothing is held");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[cfg(windows)]
mod windows {
    use std::io::Read;
    use std::process::Command;

    use super::hold_running_program;

    /// Says that this process is the copy the test started.
    const AS_THE_COPY: &str = "CRETLINK_TEST_HELD_PROGRAM";

    /// This test, as the test program takes it on its command line.
    const THIS_TEST: &str =
        "image::tests::windows::the_held_file_is_the_program_that_runs_wherever_its_file_lies";

    fn read_through(mut file: &std::fs::File) -> Vec<u8> {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read the held file");
        bytes
    }

    /// What is held is the program that runs: with its bytes, closed to every
    /// change, and also after the file was renamed away and another file was
    /// put under its path.
    #[test]
    fn the_held_file_is_the_program_that_runs_wherever_its_file_lies() {
        if std::env::var_os(AS_THE_COPY).is_some() {
            return as_the_copy();
        }
        let folder =
            std::env::temp_dir().join(format!("cretlink-held-program-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("a folder for the copy");
        let copy = folder.join("program.exe");
        std::fs::copy(std::env::current_exe().expect("this program"), &copy)
            .expect("copy this program");

        let done = Command::new(&copy)
            .args([THIS_TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(AS_THE_COPY, "1")
            .output()
            .expect("run the copy");

        let _ = std::fs::remove_dir_all(&folder);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&done.stdout),
            String::from_utf8_lossy(&done.stderr)
        );
        assert!(done.status.success(), "{said}");
        assert!(said.contains("1 passed"), "the copy ran this test: {said}");
    }

    /// While another handle may change the program file (here: rename or
    /// remove it), the program is not held: the open that shares reading
    /// only is refused. Once that handle is closed, it is held. A handle that
    /// may only read attributes would take no part in this, so the other
    /// handle asks for the right to remove the file.
    #[test]
    fn the_program_is_not_held_while_another_handle_may_change_it() {
        use std::os::windows::fs::OpenOptionsExt;
        /// `DELETE`: the right to rename or remove the file.
        const DELETE: u32 = 0x0001_0000;
        /// Reading, writing and removing shared: this handle stands in the
        /// way of nobody but a holder that shares only reading.
        const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;

        let program = std::env::current_exe().expect("this program");
        let other = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(SHARE_ALL)
            .open(&program)
            .expect("another handle that may change the file");
        assert!(
            hold_running_program().is_err(),
            "not held while another handle may change it"
        );
        drop(other);
        drop(hold_running_program().expect("held once the other handle is closed"));
    }

    /// What the copy does with its own file.
    fn as_the_copy() {
        let program = std::env::current_exe().expect("this program");
        let bytes = std::fs::read(&program).expect("read this program");

        // Held where it was started: its bytes, and nobody moves it.
        {
            let held = hold_running_program().expect("hold the program");
            assert_eq!(read_through(&held), bytes);
            assert!(
                std::fs::rename(&program, program.with_extension("aside")).is_err(),
                "a held program cannot be moved"
            );
            // Others can still read it.
            assert_eq!(std::fs::read(&program).expect("read it meanwhile"), bytes);
        }

        // Renamed away, with another file under its path: a copy by that
        // path would take the other file.
        let moved = program.with_extension("moved");
        std::fs::rename(&program, &moved).expect("a running program can be renamed");
        std::fs::write(&program, b"another program").expect("put another file under its path");

        let held = hold_running_program().expect("hold the program where it lies now");

        assert_eq!(
            read_through(&held),
            bytes,
            "the program that runs, not the file under the path it was started with"
        );
        assert!(
            std::fs::rename(&moved, program.with_extension("again")).is_err(),
            "it is held under the name it has now"
        );
        // The other file is nobody's: it is not held.
        std::fs::remove_file(&program).expect("the other file is not held");
    }
}
