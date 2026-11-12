//! `cretlink-cli recover [--remove]` (ST-M009-010): the support agent's
//! residual recovery (ST-M006-003), with the same report and the same
//! fail-closed exit codes, from the shipped command line.
//!
//! Each test points the program at its own sandbox through the environment,
//! so the real binary runs against a known machine state: `ProgramData` off
//! Windows; for a Windows program without administrator rights `LOCALAPPDATA`
//! for its own files and `ProgramData` for the folder of an earlier release.
//! An elevated Windows program asks the system where its files lie and lets
//! no environment move them (ST-M006-020), so it cannot be sandboxed: there
//! only what changes nothing on the computer is run. The same holds for a
//! Windows program without those rights on a computer where a program with
//! them has kept files: it says that it cannot see them, which is true and
//! is not the state these tests set up.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A per-test folder for what the program keeps, removed when the test ends.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cretlink-recover-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create the sandbox");
        Self(root)
    }

    /// The folder the environment names as the machine's application data.
    fn machine_data(&self) -> PathBuf {
        self.0.clone()
    }

    /// The folder the environment names as the account's application data.
    fn account_data(&self) -> PathBuf {
        self.0.join("account")
    }

    /// The ticket store of the program under test: under the account's
    /// application data on Windows, under `ProgramData` elsewhere.
    fn ticket_store(&self) -> PathBuf {
        let data = if cfg!(windows) {
            self.account_data()
        } else {
            self.machine_data()
        };
        data.join("CretLink")
            .join("reboot-resume")
            .join("tickets.json")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn recover(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cretlink-cli"))
        .arg("recover")
        .args(args)
        .env("ProgramData", sandbox.machine_data())
        .env("ALLUSERSPROFILE", sandbox.machine_data())
        .env("LOCALAPPDATA", sandbox.account_data())
        .stdin(Stdio::null())
        .output()
        .expect("run cretlink-cli recover")
}

/// Whether the program under test keeps to the sandbox, as its own report
/// tells: everywhere except as an elevated program on Windows, and except
/// where it names files of a program with administrator rights that it
/// cannot see.
fn sandboxed(inspection: &str) -> bool {
    let says = |label: &str, value: &str| {
        inspection
            .lines()
            .any(|line| line.trim_start().starts_with(label) && line.trim_end().ends_with(value))
    };
    if says("windows:", "true") && says("elevated:", "true") {
        eprintln!("an elevated Windows program cannot be sandboxed: only the inspection ran");
        return false;
    }
    if inspection.contains("run as an administrator") {
        eprintln!("a program with administrator rights kept files here: only the inspection ran");
        return false;
    }
    true
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_clean_computer_inspects_and_recovers_clean() {
    let sandbox = Sandbox::new();
    let inspect = recover(&sandbox, &[]);
    assert_eq!(inspect.status.code(), Some(0));
    let text = stdout(&inspect);
    assert!(text.contains("cretlink-cli "), "{text}");
    assert!(text.contains("effective identity"), "{text}");
    assert!(
        text.contains("CretLink owned-resource inventory (ST-M006-003):"),
        "{text}"
    );
    if !sandboxed(&text) {
        return;
    }
    assert!(text.contains("This machine is clean"), "{text}");

    let remove = recover(&sandbox, &["--remove"]);
    assert_eq!(remove.status.code(), Some(0));
    assert!(stdout(&remove).contains("nothing to remove"));
}

#[test]
fn residue_that_cannot_be_removed_fails_closed() {
    let sandbox = Sandbox::new();
    if !sandboxed(&stdout(&recover(&sandbox, &[]))) {
        return;
    }
    // A ticket store that is a directory: it cannot be read, and removing it
    // as a file fails, on every host.
    std::fs::create_dir_all(sandbox.ticket_store()).expect("make the store unreadable");

    let inspect = recover(&sandbox, &[]);
    // Inspecting removes nothing and succeeds, naming the removal command.
    assert_eq!(inspect.status.code(), Some(0));
    let text = stdout(&inspect);
    assert!(
        text.contains("! Could not read reboot-resume ticket"),
        "{text}"
    );
    assert!(
        text.contains("run:  cretlink-cli recover --remove"),
        "{text}"
    );

    let remove = recover(&sandbox, &["--remove"]);
    assert_eq!(remove.status.code(), Some(1), "{}", stdout(&remove));
    let text = stdout(&remove);
    assert!(text.contains("KEPT 1 resource(s)"), "{text}");
    assert!(text.contains("Recovery did NOT fully complete"), "{text}");
}

/// An elevated Windows program keeps its files where the system says, not
/// where its environment says: whoever starts it chooses that (ST-M006-020).
/// Residue planted in the folder the environment names is not what it looks
/// at.
#[test]
fn an_elevated_windows_program_does_not_follow_its_environment() {
    let sandbox = Sandbox::new();
    let first = stdout(&recover(&sandbox, &[]));
    let elevated_on_windows = first.lines().any(|line| {
        line.trim_start().starts_with("elevated:") && line.trim_end().ends_with("true")
    }) && cfg!(windows);
    if !elevated_on_windows {
        // Everywhere else the environment is how these tests sandbox it.
        return;
    }
    // In both folders the environment names: an unreadable ticket store, and
    // what would be the folder of an earlier release.
    for data in [sandbox.machine_data(), sandbox.account_data()] {
        std::fs::create_dir_all(
            data.join("CretLink")
                .join("reboot-resume")
                .join("tickets.json"),
        )
        .expect("plant an unreadable store");
    }

    let inspect = recover(&sandbox, &[]);

    assert_eq!(inspect.status.code(), Some(0));
    let text = stdout(&inspect);
    assert!(
        !text.contains("! Could not read reboot-resume ticket"),
        "the program looked where its environment sent it: {text}"
    );
    assert!(
        !text.contains(&sandbox.0.to_string_lossy().into_owned()),
        "the program named a folder that its environment chose: {text}"
    );
    assert!(
        text.contains("Note: this covers what CretLink keeps when it runs with administrator"),
        "{text}"
    );
}

#[test]
fn an_unknown_option_is_a_usage_error() {
    let sandbox = Sandbox::new();
    let output = recover(&sandbox, &["--force"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option '--force'"));
}
