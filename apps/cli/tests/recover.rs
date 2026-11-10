//! `cretlink-cli recover [--remove]` (ST-M009-010): the support agent's
//! residual recovery (ST-M006-003), with the same report and the same
//! fail-closed exit codes, from the shipped command line.
//!
//! Each test points the platform defaults at its own sandbox through
//! `ProgramData`, which both the staging base and the reboot-resume ticket
//! store follow, so the real binary runs against a known machine state.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A per-test `ProgramData` folder, removed when the test ends.
struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cretlink-recover-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create the sandbox");
        Self(root)
    }

    fn ticket_store(&self) -> PathBuf {
        self.0
            .join("CretLink")
            .join("reboot-resume")
            .join("tickets.json")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn recover(program_data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cretlink-cli"))
        .arg("recover")
        .args(args)
        .env("ProgramData", program_data)
        .stdin(Stdio::null())
        .output()
        .expect("run cretlink-cli recover")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_clean_computer_inspects_and_recovers_clean() {
    let sandbox = Sandbox::new();
    let inspect = recover(&sandbox.0, &[]);
    assert_eq!(inspect.status.code(), Some(0));
    let text = stdout(&inspect);
    assert!(text.contains("cretlink-cli "), "{text}");
    assert!(text.contains("effective identity"), "{text}");
    assert!(
        text.contains("CretLink owned-resource inventory (ST-M006-003):"),
        "{text}"
    );
    assert!(text.contains("This machine is clean"), "{text}");

    let remove = recover(&sandbox.0, &["--remove"]);
    assert_eq!(remove.status.code(), Some(0));
    assert!(stdout(&remove).contains("nothing to remove"));
}

#[test]
fn residue_that_cannot_be_removed_fails_closed() {
    let sandbox = Sandbox::new();
    // A ticket store that is a directory: it cannot be read, and removing it
    // as a file fails, on every host.
    std::fs::create_dir_all(sandbox.ticket_store()).expect("make the store unreadable");

    let inspect = recover(&sandbox.0, &[]);
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

    let remove = recover(&sandbox.0, &["--remove"]);
    assert_eq!(remove.status.code(), Some(1), "{}", stdout(&remove));
    let text = stdout(&remove);
    assert!(text.contains("KEPT 1 resource(s)"), "{text}");
    assert!(text.contains("Recovery did NOT fully complete"), "{text}");
}

#[test]
fn an_unknown_option_is_a_usage_error() {
    let sandbox = Sandbox::new();
    let output = recover(&sandbox.0, &["--force"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option '--force'"));
}
