//! Tests of the customer copy over a folder of their own (ST-M007-016): what
//! the folder holds, what is refused before anything is written, that nothing
//! half stays behind, and that the technician's own key never gets in.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::json;

use super::*;
use crate::relay::testing::Folder;

/// The key of the technician's own copy. No customer copy may hold it.
const TECHNICIAN_KEY: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
/// The key the technician typed for this customer.
const CUSTOMER_KEY: &str = "abababababababababababababababab";

/// The first line of a private key in the text form of a key file, written
/// in two parts so that no scanner takes this file for one that holds a key.
const KEY_MARKER: &str = concat!("-----BEGIN PRIVATE", " KEY-----\n");

/// A file that begins as a Windows program does, and is long enough to show
/// that it is copied whole.
fn program_bytes() -> Vec<u8> {
    let mut bytes = b"MZ".to_vec();
    bytes.extend((0..40_000u32).map(|n| (n % 251) as u8));
    bytes
}

/// A certificate that can be used, as the text of a PEM file.
fn certificate() -> String {
    let (cert, _key) = cretlink_wss::testcert::self_signed("relay.example.org").unwrap();
    crate::relay::testing::pem(cert.as_ref())
}

/// A technician's copy in a folder of its own, with the profile `profile`
/// next to its program, and somewhere to make customer copies.
struct Bench {
    folder: Folder,
    relay: Relay,
    program: PathBuf,
    place: PathBuf,
}

impl Bench {
    /// A relay behind an address of its own, with a certificate from the
    /// technician's own authority: every setting a copy can carry.
    fn private(test: &str) -> Self {
        let cert = certificate();
        // The authority's file as some tools write it: its private key
        // stands beside its certificate.
        let end = KEY_MARKER.replace("BEGIN", "END");
        let whole = format!("{KEY_MARKER}bm90IGEga2V5\n{end}{cert}");
        Self::with(
            test,
            &format!(
                "[relay]\naddress = \"192.0.2.1:9970\"\nname = \"relay.example.org\"\nca = \
                 \"relay-ca.pem\"\nkey = \"{TECHNICIAN_KEY}\"\n"
            ),
            Some(("relay-ca.pem", whole)),
        )
    }

    /// A relay with a certificate from a public authority: the address and
    /// the technician's key, nothing else.
    fn plain(test: &str) -> Self {
        Self::with(
            test,
            &format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"{TECHNICIAN_KEY}\"\n"),
            None,
        )
    }

    fn with(test: &str, profile: &str, ca: Option<(&str, String)>) -> Self {
        let folder = Folder::new(&format!("customer-copy-{test}"));
        folder.write("technician/cretlink.toml", profile);
        if let Some((name, pem)) = ca {
            folder.write(&format!("technician/{name}"), pem);
        }
        let Lookup::Relay(relay) = next_to_program(&folder.path("technician/cretlink")) else {
            panic!("the technician's profile can be used");
        };
        let program = folder.write("release/cretlink.exe", program_bytes());
        let place = folder.path("handed-out");
        std::fs::create_dir_all(&place).unwrap();
        Self {
            folder,
            relay: *relay,
            program,
            place,
        }
    }

    fn plan<'a>(&'a self, label: &'a str, key: Option<&'a str>) -> Plan<'a> {
        Plan {
            relay: &self.relay,
            label,
            key,
            program: &self.program,
            place: &self.place,
            replace: false,
        }
    }

    /// The names of what lies in the place copies are made in.
    fn in_place(&self) -> BTreeSet<String> {
        names_in(&self.place)
    }
}

fn names_in(folder: &Path) -> BTreeSet<String> {
    std::fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

/// Every file of a folder as text, to show what it holds and what not.
fn everything_in(folder: &Path) -> String {
    let mut all = String::new();
    for entry in std::fs::read_dir(folder).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        all.push_str(&String::from_utf8_lossy(&bytes));
        all.push('\n');
    }
    all
}

#[test]
fn the_folder_holds_the_program_the_profile_the_certificate_file_and_a_note() {
    let bench = Bench::private("content");
    let made = make(bench.plan("ACME-front-desk", Some(CUSTOMER_KEY))).unwrap();

    // One folder, named after the label, and nothing else beside it.
    let folder = bench.place.join("CretLink-Support-ACME-front-desk");
    assert_eq!(made.folder, folder);
    assert_eq!(made.set_aside, None);
    assert_eq!(
        bench.in_place(),
        names(&["CretLink-Support-ACME-front-desk"])
    );
    // Exactly the four files, as the result lists them.
    assert_eq!(
        made.files,
        [
            "CretLink-Support.exe",
            "cretlink.toml",
            "relay-ca.pem",
            "README.txt"
        ]
    );
    assert_eq!(
        names_in(&folder),
        names(&[
            "CretLink-Support.exe",
            "cretlink.toml",
            "relay-ca.pem",
            "README.txt"
        ])
    );

    // The program is the chosen file, byte for byte: a signed program stays
    // signed.
    assert_eq!(
        std::fs::read(folder.join(PROGRAM_NAME)).unwrap(),
        program_bytes()
    );
    assert_eq!(
        std::fs::read(&bench.program).unwrap(),
        program_bytes(),
        "the chosen file itself is not touched"
    );

    // The profile: the relay's address, this customer's key, the name in the
    // certificate and the certificate file.
    assert_eq!(
        std::fs::read_to_string(folder.join("cretlink.toml")).unwrap(),
        format!(
            "# The relay of this CretLink support program, for ACME-front-desk.\n\
             # Keep this file next to CretLink-Support.exe.\n\
             # A key in it stands in readable form. It only lets this copy use the relay.\n\
             [relay]\n\
             address = \"192.0.2.1:9970\"\n\
             key = \"{CUSTOMER_KEY}\"\n\
             ca = \"relay-ca.pem\"\n\
             name = \"relay.example.org\"\n"
        )
    );
    // The certificate file holds the certificates of the technician's file
    // and nothing else: the private key that stood beside them stays home.
    let kept = std::fs::read_to_string(folder.join("relay-ca.pem")).unwrap();
    assert!(kept.starts_with("-----BEGIN CERTIFICATE-----\n"), "{kept}");
    assert!(!kept.contains("PRIVATE"), "{kept}");
    // The note is for a customer on Windows.
    let note = std::fs::read_to_string(folder.join("README.txt")).unwrap();
    assert!(note.starts_with("CretLink support program\r\n"), "{note}");
    assert!(note.contains("start CretLink-Support.exe"), "{note}");
    assert!(note.contains("Nothing is installed."), "{note}");
    assert!(
        note.lines().all(|line| line.len() <= 76),
        "a line of the note is too long for a small window"
    );
    assert_eq!(note.matches('\n').count(), note.matches("\r\n").count());

    // The technician's own key is in no file of the folder.
    let all = everything_in(&folder);
    assert!(all.contains(CUSTOMER_KEY));
    assert!(
        !all.contains(TECHNICIAN_KEY),
        "the technician's key was written"
    );
}

#[test]
fn the_written_profile_is_the_one_the_customer_role_reads() {
    let bench = Bench::private("reads-back");
    let made = make(bench.plan("acme", Some(CUSTOMER_KEY))).unwrap();
    let program = made.folder.join(PROGRAM_NAME);

    // The reader of the profile file takes it, with the customer's key.
    let (file, profile) = relay_profile::read_next_to(&program)
        .unwrap()
        .expect("a profile next to the program");
    assert_eq!(file, made.folder.join("cretlink.toml"));
    assert_eq!(profile.address().to_string(), "192.0.2.1:9970");
    assert_eq!(profile.name(), Some("relay.example.org"));
    assert_eq!(profile.ca(), Some("relay-ca.pem"));
    assert_eq!(
        profile.key().map(|key| key.as_bytes().to_vec()),
        Some(CUSTOMER_KEY.as_bytes().to_vec())
    );
    // And so does the customer role, which builds the TLS settings from the
    // certificate file: the copy names the relay the technician's copy uses.
    let Lookup::Relay(relay) = next_to_program(&program) else {
        panic!("the customer role can use the copy");
    };
    assert_eq!(relay.route(), bench.relay.route());
    assert_eq!(relay.host(), "relay.example.org");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // The file with the key is for this user while it lies here.
        let mode = std::fs::metadata(made.folder.join("cretlink.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn a_copy_for_a_relay_without_a_key_or_a_certificate_file_holds_neither() {
    let bench = Bench::plain("plain");
    let made = make(bench.plan("frontdesk-2", None)).unwrap();
    assert_eq!(
        made.files,
        ["CretLink-Support.exe", "cretlink.toml", "README.txt"]
    );
    assert_eq!(
        names_in(&made.folder),
        names(&["CretLink-Support.exe", "cretlink.toml", "README.txt"])
    );
    let profile = std::fs::read_to_string(made.folder.join("cretlink.toml")).unwrap();
    assert!(
        profile.ends_with("[relay]\naddress = \"relay.example.org:443\"\n"),
        "{profile}"
    );
    // No key was typed for this customer, so the copy holds none: the
    // technician's own key is never what a copy falls back to.
    let all = everything_in(&made.folder);
    assert!(!all.contains(TECHNICIAN_KEY));
    let (_, read) = relay_profile::read_next_to(&made.folder.join(PROGRAM_NAME))
        .unwrap()
        .unwrap();
    assert!(read.key().is_none());
}

#[test]
fn a_label_is_letters_digits_and_hyphens_and_bounded() {
    let bench = Bench::plain("label");
    let too_long = "a".repeat(MAX_LABEL_CHARS + 1);
    for label in [
        "",
        " ",
        "front desk",
        "../outside",
        "a/b",
        "a\\b",
        "C:acme",
        "acme.",
        "acme_07",
        "acme.07",
        "caf\u{e9}",
        "acme\u{0}",
        "acme\n",
        too_long.as_str(),
    ] {
        let refused = make(bench.plan(label, Some(CUSTOMER_KEY))).unwrap_err();
        let CopyError::Label(message) = &refused else {
            panic!("{label:?} was not refused as a label: {refused:?}");
        };
        assert_eq!(refused.stage(), "copy_label");
        assert_eq!(
            message,
            "The label holds letters, digits and hyphens only, at least one and at most 40. It \
             becomes part of the folder's name."
        );
        // Checked before anything is written: the place is as it was.
        assert!(bench.in_place().is_empty(), "{label:?}");
    }
    // What a label may be.
    let longest = "A".repeat(MAX_LABEL_CHARS);
    for label in ["a", "ACME-07", "2026-front-desk", longest.as_str()] {
        assert!(check_label(label).is_ok(), "{label:?}");
        assert_eq!(folder_name(label), format!("CretLink-Support-{label}"));
        assert!(is_plain_windows_name(&folder_name(label)), "{label:?}");
    }
}

#[test]
fn an_existing_folder_is_never_replaced_without_asking() {
    let bench = Bench::plain("exists");
    let folder = bench.place.join("CretLink-Support-acme");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("notes.txt"),
        "what Sam wrote down for this customer",
    )
    .unwrap();

    // Without the technician's answer: refused, and nothing was changed.
    let refused = make(bench.plan("acme", Some(CUSTOMER_KEY))).unwrap_err();
    assert_eq!(refused, CopyError::Exists(folder.clone()));
    assert_eq!(refused.stage(), "copy_exists");
    assert_eq!(
        refused.to_string(),
        format!(
            "A folder {} is there already. Nothing was changed.",
            folder.display()
        )
    );
    assert_eq!(bench.in_place(), names(&["CretLink-Support-acme"]));
    assert_eq!(names_in(&folder), names(&["notes.txt"]));

    // The technician said to replace it: the folder is the new copy, whole,
    // and nothing of the folder of before or of the work in between stays.
    let made = make(Plan {
        replace: true,
        ..bench.plan("acme", Some(CUSTOMER_KEY))
    })
    .unwrap();
    assert_eq!(made.folder, folder);
    assert_eq!(made.set_aside, None);
    assert_eq!(bench.in_place(), names(&["CretLink-Support-acme"]));
    assert_eq!(
        names_in(&folder),
        names(&["CretLink-Support.exe", "cretlink.toml", "README.txt"])
    );

    // Something that is no folder has the name: it is left alone, also when
    // the technician said to replace.
    let bench = Bench::plain("exists-file");
    let file = bench.place.join("CretLink-Support-acme");
    std::fs::write(&file, "not a folder").unwrap();
    for replace in [false, true] {
        let refused = make(Plan {
            replace,
            ..bench.plan("acme", Some(CUSTOMER_KEY))
        })
        .unwrap_err();
        assert!(
            matches!(refused, CopyError::Exists(_) | CopyError::Place(_)),
            "{refused:?}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not a folder");
        assert_eq!(bench.in_place(), names(&["CretLink-Support-acme"]));
    }
}

#[test]
fn a_missing_program_or_one_that_is_no_windows_program_is_refused() {
    let bench = Bench::plain("program");
    let missing = bench.folder.path("release/gone.exe");
    let a_folder = bench.folder.path("release");
    let script = bench
        .folder
        .write("release/cretlink", "#!/bin/sh\necho no\n");
    let empty = bench.folder.write("release/empty.exe", "");
    for (program, says) in [
        (&missing, "The program to copy is not there."),
        (&a_folder, "The program to copy is not there."),
        (&script, "That file is not a Windows program."),
        (&empty, "That file is not a Windows program."),
    ] {
        let refused = make(Plan {
            program,
            ..bench.plan("acme", Some(CUSTOMER_KEY))
        })
        .unwrap_err();
        assert_eq!(refused.stage(), "copy_program", "{refused:?}");
        assert!(refused.to_string().starts_with(says), "{refused}");
        assert!(
            refused
                .to_string()
                .contains("Windows archive of a CretLink release"),
            "{refused}"
        );
        assert!(bench.in_place().is_empty());
    }
}

#[test]
fn a_place_that_is_not_there_is_refused() {
    let bench = Bench::plain("place");
    let gone = bench.folder.path("no-such-folder");
    let a_file = bench.folder.write("a-file", "x");
    for place in [
        gone.as_path(),
        a_file.as_path(),
        Path::new("relative/place"),
    ] {
        let refused = make(Plan {
            place,
            ..bench.plan("acme", Some(CUSTOMER_KEY))
        })
        .unwrap_err();
        assert_eq!(refused.stage(), "copy_place", "{refused:?}");
    }
    assert!(!gone.exists());
}

#[test]
fn a_key_that_cannot_be_a_key_is_refused_and_not_repeated() {
    let bench = Bench::plain("key");
    let too_long = "k".repeat(relay_profile::MAX_KEY_BYTES + 1);
    for (key, says) in [
        (
            "",
            "Type the admission key of this customer or generate one, or say that the relay asks \
             for no key.",
        ),
        (
            "two words for a key",
            "The key holds a blank or a character that is not plain text.",
        ),
        (
            "tab\tinside",
            "The key holds a blank or a character that is not plain text.",
        ),
        (too_long.as_str(), "The key is longer than 512 characters."),
    ] {
        let refused = make(bench.plan("acme", Some(key))).unwrap_err();
        assert_eq!(refused.stage(), "copy_key", "{refused:?}");
        let said = refused.to_string();
        assert_eq!(said, says);
        if key.len() > 3 {
            assert!(!said.contains(key), "{said}");
        }
        assert!(bench.in_place().is_empty());
    }
}

#[test]
fn a_copy_that_does_not_take_its_place_leaves_nothing_behind() {
    let bench = Bench::plain("half");
    let target = bench.place.join("CretLink-Support-acme");

    // Written and read back, the folder has another name: nothing in the
    // place looks like a customer copy yet.
    let staged = stage(bench.plan("acme", Some(CUSTOMER_KEY))).unwrap();
    assert!(!target.exists());
    assert_eq!(
        bench.in_place(),
        names(&["CretLink-Support-acme.cretlink-new"])
    );
    assert!(staged.stage().join(PROGRAM_NAME).is_file());
    // Somebody makes a folder of that name in this very moment. The copy
    // does not take its place, that folder is left alone, and what was
    // written is removed.
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("theirs.txt"), "not ours").unwrap();
    let refused = staged.place().unwrap_err();
    assert_eq!(refused, CopyError::Exists(target.clone()));
    assert_eq!(bench.in_place(), names(&["CretLink-Support-acme"]));
    assert_eq!(names_in(&target), names(&["theirs.txt"]));

    // A copy that was begun and given up is removed as well.
    std::fs::remove_dir_all(&target).unwrap();
    let staged = stage(bench.plan("acme", Some(CUSTOMER_KEY))).unwrap();
    drop(staged);
    assert!(bench.in_place().is_empty());

    // What a run that was interrupted left behind is no obstacle and is not
    // taken for part of the new copy.
    let left = bench.place.join("CretLink-Support-acme.cretlink-new");
    std::fs::create_dir_all(&left).unwrap();
    std::fs::write(left.join("half-written"), "x").unwrap();
    let made = make(bench.plan("acme", Some(CUSTOMER_KEY))).unwrap();
    assert_eq!(bench.in_place(), names(&["CretLink-Support-acme"]));
    assert_eq!(
        names_in(&made.folder),
        names(&["CretLink-Support.exe", "cretlink.toml", "README.txt"])
    );
}

#[test]
fn a_failure_says_whether_anything_was_left_and_never_holds_the_key() {
    let nothing = CopyError::Write {
        message: "the disk is full".to_owned(),
        left: None,
    };
    assert_eq!(nothing.stage(), "copy_write");
    assert_eq!(
        nothing.to_string(),
        "The customer copy could not be written: the disk is full. Nothing of it was left \
         behind."
    );
    let something = CopyError::Write {
        message: "the disk is full".to_owned(),
        left: Some(PathBuf::from("CretLink-Support-acme.cretlink-new")),
    };
    assert_eq!(
        something.to_string(),
        "The customer copy could not be written: the disk is full. What was written could not \
         be removed again and lies in CretLink-Support-acme.cretlink-new; it is not a copy that \
         can be used, so remove it yourself."
    );
    // What is printed of a plan and of a form holds no key.
    let bench = Bench::plain("printed");
    let printed = format!("{:?}", bench.plan("acme", Some(CUSTOMER_KEY)));
    assert!(printed.contains("has_key: true"), "{printed}");
    assert!(!printed.contains(CUSTOMER_KEY) && !printed.contains(TECHNICIAN_KEY));
}

#[test]
fn every_name_that_goes_into_a_path_is_one_plain_name_that_windows_takes() {
    for name in [
        "relay-ca.pem",
        "Our relay (2026).crt",
        "CretLink-Support-acme",
        "authority",
    ] {
        assert!(is_plain_windows_name(name), "{name:?}");
    }
    let too_long = format!("{}.pem", "a".repeat(relay_profile::MAX_CA_NAME_CHARS));
    for name in [
        "",
        ".",
        "..",
        "certs/relay-ca.pem",
        "certs\\relay-ca.pem",
        "C:relay-ca.pem",
        "relay-ca.pem.",
        "relay-ca.pem ",
        " relay-ca.pem",
        "relay<ca>.pem",
        "relay|ca.pem",
        "relay?.pem",
        "relay*.pem",
        "relay\"ca\".pem",
        "relay\u{0}ca.pem",
        "nul",
        "NUL.pem",
        "com1.crt",
        "Lpt9.tar.gz",
        too_long.as_str(),
    ] {
        assert!(!is_plain_windows_name(name), "{name:?}");
    }
}

/// A relay whose certificate file has a name that a copy for Windows cannot
/// hold, or the name of one of the copy's own files: the copy is refused
/// before anything is written. Such a file can only be set up where the
/// system takes the name, so this runs where file names are free.
#[cfg(unix)]
#[test]
fn a_certificate_file_with_a_name_that_a_copy_cannot_hold_is_refused() {
    for ca in [
        "nul.pem",
        "README.txt",
        "CRETLINK.TOML.",
        "cretlink-support.EXE",
    ] {
        let bench = Bench::with(
            "names",
            &format!("[relay]\naddress = \"relay.example.org:443\"\nca = \"{ca}\"\n"),
            Some((ca, certificate())),
        );
        let refused = make(bench.plan("acme", Some(CUSTOMER_KEY))).unwrap_err();
        assert_eq!(refused.stage(), "copy_relay", "{ca}: {refused:?}");
        assert!(
            refused
                .to_string()
                .contains("a name that a customer copy cannot hold"),
            "{refused}"
        );
        assert!(bench.in_place().is_empty(), "{ca}");
    }
}

#[test]
fn a_generated_key_fits_a_profile_and_a_line_of_the_relays_key_file() {
    let first = generate_customer_key().unwrap().key;
    let second = generate_customer_key().unwrap().key;
    assert_ne!(first, second, "two keys from the system's random source");
    for key in [&first, &second] {
        assert_eq!(key.len(), 64);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()), "{key}");
        // It is a key of a relay profile.
        assert!(RelayProfile::from_fields("relay.example.org:443", Some(key), None, None).is_ok());
    }
    // The line the window shows, `label:key`, is one the relay's key file
    // takes, for every label a copy can have.
    let longest = "A".repeat(MAX_LABEL_CHARS);
    for label in ["acme", "ACME-front-desk-07", "-", longest.as_str()] {
        check_label(label).unwrap();
        let line = format!("{label}:{first}\n");
        assert!(
            cretlink_relay_server::keys::parse_keys(&line).is_ok(),
            "{label}"
        );
    }
    // What is printed of a generated key is not the key.
    let printed = format!("{:?}", GeneratedKey { key: first.clone() });
    assert!(!printed.contains(&first), "{printed}");
}

#[test]
fn the_form_names_what_was_chosen_by_a_number_and_cannot_ask_for_the_technicians_key() {
    let bench = Bench::plain("form");
    let picks = Picks::default();
    let form = |patch: serde_json::Value| -> CustomerCopyForm {
        let mut form = json!({ "label": "acme", "keyMode": "set", "key": CUSTOMER_KEY });
        for (name, value) in patch.as_object().unwrap() {
            form[name] = value.clone();
        }
        serde_json::from_value(form).unwrap()
    };
    let make = |form: &CustomerCopyForm, source: ProgramSource| {
        make_from_form(&bench.relay, form, &picks, source, &bench.program)
    };

    // Nothing was chosen yet: the place is asked for, and where the
    // technician chooses the program, the program.
    let nothing = form(json!({}));
    assert_eq!(
        make(&nothing, ProgramSource::ThisProgram)
            .unwrap_err()
            .stage(),
        "copy_place"
    );
    assert_eq!(
        make(&nothing, ProgramSource::Choose).unwrap_err().stage(),
        "copy_program"
    );
    // The label is checked first: it is the first thing on the form.
    assert_eq!(
        make(&form(json!({ "label": "a b" })), ProgramSource::Choose)
            .unwrap_err()
            .stage(),
        "copy_label"
    );

    // A number that names nothing, or an earlier choice, is no choice.
    let stale = picks.keep_place(bench.folder.path("elsewhere"));
    let place = picks.keep_place(bench.place.clone());
    assert_eq!(
        make(
            &form(json!({ "placePick": stale })),
            ProgramSource::ThisProgram
        )
        .unwrap_err()
        .stage(),
        "copy_place"
    );
    assert!(bench.in_place().is_empty());

    // On Windows the copy is this program itself; elsewhere the chosen file.
    let made = make(
        &form(json!({ "placePick": place })),
        ProgramSource::ThisProgram,
    )
    .unwrap();
    assert_eq!(
        made.folder,
        bench
            .place
            .join("CretLink-Support-acme")
            .display()
            .to_string()
    );
    assert_eq!(
        made.files,
        ["CretLink-Support.exe", "cretlink.toml", "README.txt"]
    );
    assert_eq!(
        serde_json::to_value(&made).unwrap(),
        json!({ "copy": made.copy, "folder": made.folder, "files": made.files })
    );
    // Only the copy that was made can be shown, by its number.
    assert_eq!(
        picks.made(made.copy),
        Some(bench.place.join("CretLink-Support-acme"))
    );
    assert_eq!(picks.made(made.copy + 1), None);

    let other = bench.folder.write("release/other.exe", program_bytes());
    let program = picks.keep_program(other);
    let chosen = form(json!({ "label": "frontdesk", "placePick": place, "programPick": program }));
    let made = make(&chosen, ProgramSource::Choose).unwrap();
    assert!(made.folder.ends_with("CretLink-Support-frontdesk"));
    // Asked again without an answer: the folder exists, and it stays.
    assert_eq!(
        make(&chosen, ProgramSource::Choose).unwrap_err().stage(),
        "copy_exists"
    );
    let replaced = form(json!({
        "label": "frontdesk", "placePick": place, "programPick": program, "replace": true,
    }));
    assert!(make(&replaced, ProgramSource::Choose).is_ok());

    // A copy without a key, said as such.
    let none =
        form(json!({ "label": "nokey", "keyMode": "none", "key": null, "placePick": place }));
    let made = make(&none, ProgramSource::ThisProgram).unwrap();
    let written = everything_in(Path::new(&made.folder));
    assert!(!written.contains("key = "), "{written}");
    assert!(!written.contains(TECHNICIAN_KEY));
    // A key is asked for when the form says there is one.
    let empty = form(json!({ "label": "empty", "key": null, "placePick": place }));
    assert_eq!(
        make(&empty, ProgramSource::ThisProgram)
            .unwrap_err()
            .stage(),
        "copy_key"
    );

    // The form has no way to say "the technician's own key", and takes no
    // path of this computer.
    for bad in [
        json!({ "label": "acme", "keyMode": "keep" }),
        json!({ "label": "acme", "keyMode": "own" }),
        json!({ "label": "acme", "keyMode": "none", "place": "/home/sam" }),
        json!({ "label": "acme", "keyMode": "none", "program": "/home/sam/cretlink.exe" }),
        json!({ "keyMode": "none" }),
    ] {
        assert!(serde_json::from_value::<CustomerCopyForm>(bad).is_err());
    }
    let printed = format!("{:?}", form(json!({})));
    assert!(printed.contains("has_key: true"), "{printed}");
    assert!(!printed.contains(CUSTOMER_KEY), "{printed}");
}

#[test]
fn the_options_show_the_relay_of_a_copy_without_any_key() {
    let bench = Bench::private("options");
    let options = CustomerCopyOptions::of(&bench.relay);
    assert_eq!(
        serde_json::to_value(&options).unwrap(),
        json!({
            "relay": {
                "host": "relay.example.org",
                "address": "192.0.2.1:9970",
                "trust": "profile_file",
                // The technician's copy holds a key; a customer copy does
                // not get it, and the view does not say that one is set.
                "keySet": false,
                "ca": "relay-ca.pem",
                "name": "relay.example.org",
            },
            "program": if cfg!(windows) { "this_program" } else { "choose" },
            "labelMax": 40,
            "folderPrefix": "CretLink-Support-",
            "programName": "CretLink-Support.exe",
        })
    );
    assert!(!format!("{options:?}").contains(TECHNICIAN_KEY));
}
