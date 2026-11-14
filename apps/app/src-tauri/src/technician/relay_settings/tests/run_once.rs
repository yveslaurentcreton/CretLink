//! Settings → Relay in a technician copy that runs once (ST-M007-011): the
//! form is filled from the file `cretlink.toml` next to the program, Save
//! writes that file and the certificate file beside it, Remove removes them,
//! and nothing else is written, by this copy or by a copy for a customer.

use std::path::PathBuf;

use super::*;
use cretlink_cli::relay_profile::program_file::KEY_PLACEHOLDER;

/// A plainly fake key for these tests.
const SHORT_KEY: &str = "key-for-tests";

/// A copy that runs once, with the folder of its program made.
fn run_once(home: &Home) -> (ThisCopy, PathBuf) {
    let copy = home.run_once();
    let folder = copy.exe.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&folder).unwrap();
    let file = relay_profile::path_next_to(&copy.exe).unwrap();
    (copy, file)
}

/// The names of the files in a folder, sorted.
fn names(folder: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn the_form_of_a_copy_that_runs_once_is_the_file_next_to_its_program() {
    let home = Home::new("run-once-view");
    let (copy, file) = run_once(&home);
    let view = RelaySettingsView::of(&copy);
    assert!(view.editable);
    assert_eq!(view.kept_in, "program_folder");
    assert_eq!(view.source, "none");
    assert_eq!(view.file.path, file.display().to_string());
    // What Save writes is that file, and the screen names where the
    // certificate file goes before anything is saved.
    assert_eq!(view.stored.as_ref(), Some(&view.file));
    assert_eq!(
        view.ca_path.as_deref(),
        Some(
            file.with_file_name("relay-ca.pem")
                .display()
                .to_string()
                .as_str()
        )
    );
    let shown = serde_json::to_value(&view).unwrap();
    assert_eq!(shown["keptIn"], "program_folder");
    // Looking wrote nothing.
    assert!(names(file.parent().unwrap()).is_empty());

    // A file that is there fills the form.
    home.folder.write(
        file.to_str().unwrap(),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"{SHORT_KEY}\"\n"),
    );
    let view = RelaySettingsView::of(&copy);
    assert_eq!(view.source, "file");
    let relay = view.stored.unwrap().relay.unwrap();
    assert_eq!(relay.address, "relay.example.org:443");
    assert!(relay.key_set);
    assert!(!serde_json::to_string(&relay).unwrap().contains(SHORT_KEY));
}

#[test]
fn save_writes_the_file_next_to_the_program_and_nothing_else() {
    let home = Home::new("run-once-save");
    let (copy, file) = run_once(&home);
    let pending = PendingCa::default();
    let pem = certificate();

    let mut first = with_key(form("192.0.2.1:9970"), SHORT_KEY);
    first.name = Some(RELAY_NAME.to_owned());
    first.ca_mode = CaMode::Chosen;
    first.ca_pick = Some(pending.keep(pem.clone()));
    let view = save(&copy, &first, &pending).unwrap();
    // The two files, beside the program, and nothing anywhere else: no
    // settings, no install record.
    let folder = file.parent().unwrap();
    assert_eq!(names(folder), ["cretlink.toml", "relay-ca.pem"]);
    let everything: Vec<PathBuf> = home
        .folder
        .snapshot()
        .into_iter()
        .map(|(path, _)| path)
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(everything.len(), 2, "{everything:?}");
    assert_eq!(std::fs::read(folder.join("relay-ca.pem")).unwrap(), pem);
    // The answer, and the copy as it opens next time, show the relay with
    // its key as set, never the key.
    for view in [view, RelaySettingsView::of(&copy)] {
        assert_eq!(view.source, "file");
        let relay = view.stored.unwrap().relay.unwrap();
        assert_eq!((relay.host.as_str(), relay.key_set), (RELAY_NAME, true));
        assert_eq!(relay.ca.as_deref(), Some("relay-ca.pem"));
    }
    // The session of this copy goes through it, with its key.
    let relay = relay_of_copy(&copy).unwrap();
    assert_eq!(
        relay.profile().key().unwrap().as_bytes(),
        SHORT_KEY.as_bytes()
    );

    // A kept key stays with its relay, as in the installed copy.
    let mut keep = form("192.0.2.1:443");
    keep.name = Some(RELAY_NAME.to_owned());
    save(&copy, &keep, &pending).unwrap();
    assert!(relay_of_copy(&copy).unwrap().profile().key().is_some());
    let mut elsewhere = keep.clone();
    elsewhere.name = Some("other.example.org".to_owned());
    let refused = save(&copy, &elsewhere, &pending).unwrap_err();
    assert_eq!(refused.stage, "relay_key");

    // Remove takes both files.
    let view = remove(&copy, &pending).unwrap();
    assert_eq!(view.source, "none");
    assert!(names(folder).is_empty());
}

#[test]
fn a_hand_written_file_keeps_its_comments_and_its_own_certificate_file() {
    let home = Home::new("run-once-hand");
    let (copy, file) = run_once(&home);
    let pending = PendingCa::default();
    let pem = certificate();
    let folder = file.parent().unwrap().to_path_buf();
    home.folder
        .write(folder.join("own-ca.pem").to_str().unwrap(), &pem);
    home.folder.write(
        file.to_str().unwrap(),
        format!(
            "# Written by hand\n[relay]\naddress = \"192.0.2.1:9970\"\nname = \"{RELAY_NAME}\"\n\
             ca = \"own-ca.pem\"\n"
        ),
    );
    // Kept as it is: its certificates are written anew under the one name.
    let mut keep = form("192.0.2.1:9971");
    keep.name = Some(RELAY_NAME.to_owned());
    save(&copy, &keep, &pending).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("# Written by hand\n[relay]\n"), "{text}");
    assert!(text.contains("ca = \"relay-ca.pem\""), "{text}");
    assert_eq!(std::fs::read(folder.join("relay-ca.pem")).unwrap(), pem);

    // Remove leaves the file that the hand-written profile named.
    remove(&copy, &pending).unwrap();
    assert_eq!(names(&folder), ["own-ca.pem"]);
}

#[test]
fn a_file_that_cannot_be_read_is_not_replaced_and_remove_takes_it() {
    let home = Home::new("run-once-wrong");
    let (copy, file) = run_once(&home);
    let pending = PendingCa::default();
    let wrong = format!("[relay]\naddress = \"relay.example.org\"\nkey = \"{SHORT_KEY}\"\n");
    home.folder.write(file.to_str().unwrap(), &wrong);
    let view = RelaySettingsView::of(&copy);
    assert!(view.stored.unwrap().problem.is_some());

    let refused = save(&copy, &form("relay.example.org:443"), &pending).unwrap_err();
    assert_eq!(refused.stage, "relay_profile");
    assert!(refused.message.contains("Remove"), "{}", refused.message);
    assert!(!refused.message.contains(SHORT_KEY));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), wrong);

    remove(&copy, &pending).unwrap();
    assert!(!file.exists());
}

#[test]
fn a_folder_that_cannot_be_written_gets_the_text_for_the_file_without_the_key() {
    let home = Home::new("run-once-unwritable");
    // The program's folder is a file: no system makes a file in it, also
    // not for an administrator.
    let in_the_way = home.folder.write("home/Downloads", "not a folder");
    let copy = home.run_once();
    let pending = PendingCa::default();
    let mut full = with_key(form("192.0.2.1:9970"), SHORT_KEY);
    full.name = Some(RELAY_NAME.to_owned());
    let refused = save(&copy, &full, &pending).unwrap_err();
    assert_eq!(refused.stage, "relay_folder");
    assert!(
        refused.message.contains("cannot be written"),
        "{}",
        refused.message
    );
    assert!(
        refused.message.contains("Nothing was changed"),
        "{}",
        refused.message
    );
    let text = refused.file_text.clone().unwrap();
    assert_eq!(
        text,
        format!(
            "[relay]\naddress = \"192.0.2.1:9970\"\n{KEY_PLACEHOLDER}\nname = \"{RELAY_NAME}\"\n"
        )
    );
    let sent = serde_json::to_string(&refused).unwrap();
    assert!(!sent.contains(SHORT_KEY), "{sent}");
    assert!(sent.contains("fileText"), "{sent}");
    assert_eq!(
        std::fs::read_to_string(&in_the_way).unwrap(),
        "not a folder"
    );
}

#[test]
fn a_copy_for_a_customer_writes_no_relay() {
    let home = Home::new("run-once-customer");
    let copy = ThisCopy {
        exe: home.folder.path("support/CretLink-Support.exe"),
        installed: None,
    };
    std::fs::create_dir_all(copy.exe.parent().unwrap()).unwrap();
    let pending = PendingCa::default();
    let view = RelaySettingsView::of(&copy);
    assert!(!view.editable);
    assert_eq!((view.stored, view.ca_path), (None, None));
    for refused in [
        save(
            &copy,
            &with_key(form("relay.example.org:443"), SHORT_KEY),
            &pending,
        ),
        remove(&copy, &pending),
    ] {
        assert_eq!(refused.unwrap_err().stage, "relay_customer_copy");
    }
    assert!(names(copy.exe.parent().unwrap()).is_empty());

    // And the customer role has none of these commands: its window cannot
    // ask for them.
    let customer = include_str!("../../../customer/mod.rs");
    for command in ["save_relay", "remove_relay", "relay_settings"] {
        assert!(!customer.contains(command), "{command}");
    }
}

#[test]
fn a_form_of_a_copy_that_runs_once_is_tested_without_writing() {
    let home = Home::new("run-once-test");
    let (copy, file) = run_once(&home);
    let relay = relay_to_test(
        &copy,
        Some(&with_key(form("relay.example.org:443"), SHORT_KEY)),
        &PendingCa::default(),
    )
    .unwrap();
    assert_eq!(relay.host(), "relay.example.org");
    assert!(names(file.parent().unwrap()).is_empty());
}
