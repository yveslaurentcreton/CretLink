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
//!
//! The webview profile folders that the application's window left
//! (ST-M009-014) lie in the temporary folder and in the data folder of the
//! account, with or without administrator rights. Each test gives the program
//! a temporary folder and a home of its own, so that it never looks at, or
//! removes, what lies in the real ones.

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
        std::fs::create_dir_all(root.join("tmp")).expect("create the sandbox");
        std::fs::create_dir_all(root.join("home")).expect("create the sandbox");
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

    /// The folder the environment names as the temporary folder.
    fn temp(&self) -> PathBuf {
        self.0.join("tmp")
    }

    /// The folder the environment names as the home of the account.
    fn home(&self) -> PathBuf {
        self.0.join("home")
    }

    /// Where earlier versions kept the webview profile of the account; `None`
    /// on macOS, where the webview takes no folder.
    fn earlier_webview_profile(&self) -> Option<PathBuf> {
        if cfg!(windows) {
            Some(self.account_data().join("dev.cretlink.app"))
        } else if cfg!(target_os = "macos") {
            None
        } else {
            Some(
                self.home()
                    .join(".local")
                    .join("share")
                    .join("dev.cretlink.app"),
            )
        }
    }

    /// A webview profile folder in the temporary folder, as the copy number
    /// `number` made it: with its lock file and what a webview wrote.
    fn webview_profile(&self, number: u8) -> PathBuf {
        let dir = self.temp().join(format!("CretLink-webview-{number:032x}"));
        std::fs::create_dir_all(dir.join("Cache")).expect("make the profile folder");
        std::fs::write(dir.join("cretlink.lock"), b"").expect("make its lock file");
        std::fs::write(dir.join("Cache").join("index"), b"x").expect("fill it");
        dir
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
        // The places of the webview profile folders (ST-M009-014).
        .env("TMPDIR", sandbox.temp())
        .env("TMP", sandbox.temp())
        .env("TEMP", sandbox.temp())
        .env("HOME", sandbox.home())
        .env("USERPROFILE", sandbox.home())
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null())
        .output()
        .expect("run cretlink-cli recover")
}

/// Whether the program under test runs as an elevated program on Windows, as
/// its own report tells.
fn elevated_on_windows(inspection: &str) -> bool {
    let says = |label: &str, value: &str| {
        inspection
            .lines()
            .any(|line| line.trim_start().starts_with(label) && line.trim_end().ends_with(value))
    };
    says("windows:", "true") && says("elevated:", "true")
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

/// The webview profile folders that the application's window left are part
/// of the same report (ST-M009-014): the folder of a copy that no longer
/// runs and the folder of an earlier version are listed and removed, and the
/// folder of a copy that runs is neither.
#[test]
fn webview_folders_that_were_left_are_listed_and_removed() {
    let sandbox = Sandbox::new();
    let clean = stdout(&recover(&sandbox, &[]));
    assert!(
        clean.contains("webview profile"),
        "the kind is looked for: {clean}"
    );
    let stale = sandbox.webview_profile(1);
    let running = sandbox.webview_profile(2);
    // A copy that runs holds the lock of its folder.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(running.join("cretlink.lock"))
        .expect("open the lock of the running copy");
    lock.try_lock().expect("hold the lock of the running copy");
    let earlier = sandbox.earlier_webview_profile();
    if let Some(earlier) = &earlier {
        std::fs::create_dir_all(earlier.join("Cache")).expect("plant the earlier profile");
    }

    let inspect = recover(&sandbox, &[]);

    assert_eq!(inspect.status.code(), Some(0));
    let text = stdout(&inspect);
    // A program with administrator rights looks at nothing that its
    // environment names (REQ-S-024): the folders of the account are left to
    // a run without those rights, and the report says so.
    if elevated_on_windows(&text) {
        assert!(!text.contains(&stale.display().to_string()), "{text}");
        assert!(
            text.contains("the webview folders in the temporary"),
            "the report says what it does not cover: {text}"
        );
        assert!(
            stale.join("Cache").exists(),
            "nothing of the account is touched"
        );
        return;
    }
    assert!(
        text.contains(&format!(
            "  - webview profile: {} (left by a copy that no longer runs)",
            stale.display()
        )),
        "{text}"
    );
    if let Some(earlier) = &earlier {
        assert!(
            text.contains(&format!(
                "  - webview profile: {} (left by an earlier version)",
                earlier.display()
            )),
            "{text}"
        );
    }
    assert!(
        !text.contains(&running.display().to_string()),
        "the folder of a copy that runs is not a leftover: {text}"
    );
    assert!(
        text.contains("run:  cretlink-cli recover --remove"),
        "{text}"
    );
    assert!(
        stale.join("Cache").exists(),
        "an inspection removes nothing"
    );
    // The removal runs for real only where every place is the sandbox's: an
    // elevated Windows program also removes what lies in the machine's own
    // places, which another test of this file plants there.
    if !sandboxed(&text) {
        return;
    }

    let remove = recover(&sandbox, &["--remove"]);

    let text = stdout(&remove);
    assert_eq!(remove.status.code(), Some(0), "{text}");
    assert!(
        text.contains(&format!("  - removed webview profile: {}", stale.display())),
        "{text}"
    );
    assert!(
        text.contains("Recovery complete — this machine is clean."),
        "{text}"
    );
    assert!(!stale.exists());
    assert!(earlier.is_none_or(|earlier| !earlier.exists()));
    assert!(
        running.join("Cache").join("index").exists(),
        "the folder of a copy that runs is never touched"
    );
    drop(lock);
}

#[test]
fn an_unknown_option_is_a_usage_error() {
    let sandbox = Sandbox::new();
    let output = recover(&sandbox, &["--force"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown option '--force'"));
}

/// In continuous verification the program runs on Windows as an elevated
/// administrator, in the real places of the machine, which no sandbox
/// replaces. There the recovery is run for real: the folder of an earlier
/// release, with a manifest that names a file elsewhere, is listed and
/// removed whole, and the file it names stays (ST-M006-020).
#[test]
fn in_verification_an_elevated_recovery_removes_the_folder_of_an_earlier_release() {
    if !cfg!(windows) || std::env::var_os("CI").is_none() {
        return;
    }
    let sandbox = Sandbox::new();
    let first = stdout(&recover(&sandbox, &[]));
    let elevated = first.lines().any(|line| {
        line.trim_start().starts_with("elevated:") && line.trim_end().ends_with("true")
    });
    if !elevated {
        return;
    }
    // The file that a planted manifest names, and the folder of an earlier
    // release on the drive of Windows with that manifest in it.
    let named = sandbox.0.join("important.txt");
    std::fs::write(&named, b"important").expect("write the named file");
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let earlier = PathBuf::from(format!("{drive}\\ProgramData\\CretLink"));
    let session = earlier
        .join("staging")
        .join("11111111-2222-4333-8444-555555555555");
    std::fs::create_dir_all(&session).expect("plant the session folder");
    let manifest = format!(
        r#"{{"version":1,"staged":[{{"canonical_path":{:?},"source":"x","len":0,"algo":"sha256","digest":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","cleanup":"on_session_end","created_ms":0}}]}}"#,
        named.to_string_lossy()
    );
    std::fs::write(session.join("manifest.json"), manifest).expect("plant the manifest");

    let inspect = stdout(&recover(&sandbox, &[]));
    assert!(
        inspect.contains("folder of an earlier release"),
        "{inspect}"
    );
    assert!(named.exists(), "an inspection removes nothing");

    let remove = stdout(&recover(&sandbox, &["--remove"]));

    assert!(
        !earlier.exists(),
        "the folder of the earlier release is gone: {remove}"
    );
    assert_eq!(
        std::fs::read(&named).expect("the named file is still there"),
        b"important"
    );
}
