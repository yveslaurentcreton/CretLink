//! Tests of the relay file next to the program (ST-M007-011): what Save
//! writes, what it keeps of a file written by hand, what Remove removes, and
//! what is refused.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::relay_profile::{PROFILE_FILE, read_file};

/// A plainly fake key for tests.
const KEY: &str = "key-for-tests";

/// A certificate file in the form it is kept in. Its one block is no real
/// certificate: whether a certificate can be used is decided where the TLS
/// settings are built.
const PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

static NEXT: AtomicU64 = AtomicU64::new(0);

/// The folder of a program, of its own for one test, removed afterwards.
struct Folder(PathBuf);

impl Folder {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cretlink-program-file-{test}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn profile(&self) -> PathBuf {
        self.0.join(PROFILE_FILE)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn relay(key: Option<&str>, ca: Option<&str>) -> RelayProfile {
    RelayProfile::from_fields("relay.example.org:443", key, ca, None).unwrap()
}

#[test]
fn save_writes_the_profile_and_its_certificate_file_beside_the_program() {
    let folder = Folder::new("save");
    let path = folder.profile();
    write(
        &path,
        &relay(Some(KEY), Some(CA_FILE)),
        RelayCa::Replace(PEM),
    )
    .unwrap();
    assert_eq!(folder.names(), [PROFILE_FILE, CA_FILE]);
    // Every copy reads it back as it was saved.
    let read = read_file(&path).unwrap().unwrap();
    assert_eq!(read.address().to_string(), "relay.example.org:443");
    assert_eq!(read.key().unwrap().as_bytes(), KEY.as_bytes());
    assert_eq!(read.ca(), Some(CA_FILE));
    assert_eq!(fs::read(ca_path(&path)).unwrap(), PEM);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for file in [&path, &ca_path(&path)] {
            let mode = fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }
    }

    // Without a certificate file, the one beside it goes with the change.
    write(&path, &relay(None, None), RelayCa::Remove).unwrap();
    assert_eq!(folder.names(), [PROFILE_FILE]);
    assert!(read_file(&path).unwrap().unwrap().key().is_none());
}

#[test]
fn save_keeps_what_a_person_wrote_around_the_relay() {
    let section = relay(None, None).to_section();
    // Comments before, after and inside the section stay where they stood;
    // the settings are replaced under the header.
    let before = "# For the laptop in the van\n\n[relay] # ours\n# the old relay\naddress = \
                  \"old.example.org:9970\"\nkey = 'key-for-tests'\n# kept as well\n";
    assert_eq!(
        merged(Some(before), &section).as_str(),
        "# For the laptop in the van\n\n[relay] # ours\naddress = \"relay.example.org:443\"\n\
         # the old relay\n# kept as well\n"
    );
    // A file of comments only gets a section at its end.
    assert_eq!(
        merged(Some("# only a comment"), &section).as_str(),
        "# only a comment\n\n[relay]\naddress = \"relay.example.org:443\"\n"
    );
    // No file: the section alone.
    assert_eq!(merged(None, &section).as_str(), section.as_str());
    // The line ends of the file are kept.
    assert_eq!(
        merged(
            Some("# a\r\n[relay]\r\naddress = \"a.example.org:1\"\r\n"),
            &section
        )
        .as_str(),
        "# a\r\n[relay]\r\naddress = \"relay.example.org:443\"\r\n"
    );
    // What comes out is a profile again.
    let folder = Folder::new("merge");
    fs::write(folder.profile(), before).unwrap();
    write(&folder.profile(), &relay(Some(KEY), None), RelayCa::Remove).unwrap();
    let text = fs::read_to_string(folder.profile()).unwrap();
    assert!(text.starts_with("# For the laptop in the van\n"), "{text}");
    assert!(text.contains("# kept as well"), "{text}");
    assert!(read_file(&folder.profile()).unwrap().is_some());
}

#[test]
fn a_file_that_cannot_be_read_is_not_overwritten_and_remove_takes_it() {
    let folder = Folder::new("unreadable");
    let path = folder.profile();
    let wrong = "[relay]\naddress = \"relay.example.org\"\n# written by hand\n";
    fs::write(&path, wrong).unwrap();
    let error = write(&path, &relay(None, None), RelayCa::Remove).unwrap_err();
    assert!(matches!(error, WriteError::Unreadable(_)), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), wrong);

    remove(&path).unwrap();
    assert!(folder.names().is_empty());
}

#[test]
fn remove_takes_the_profile_and_the_one_certificate_file_of_that_name() {
    let folder = Folder::new("remove");
    let path = folder.profile();
    // Written by hand, with a certificate file of another name.
    fs::write(
        &path,
        "[relay]\naddress = \"relay.example.org:443\"\nca = \"own-ca.pem\"\n",
    )
    .unwrap();
    fs::write(folder.0.join("own-ca.pem"), PEM).unwrap();
    fs::write(ca_path(&path), PEM).unwrap();
    remove(&path).unwrap();
    // The file of the hand-written profile stays; the one of Save goes.
    assert_eq!(folder.names(), ["own-ca.pem"]);
    // Nothing there: nothing to do.
    remove(&path).unwrap();
}

#[test]
fn nothing_is_written_or_removed_through_something_that_is_not_a_plain_file() {
    let folder = Folder::new("not-plain");
    let path = folder.profile();
    // A folder where the certificate file would go.
    fs::create_dir_all(ca_path(&path)).unwrap();
    fs::write(&path, "[relay]\naddress = \"relay.example.org:443\"\n").unwrap();
    for refused in [
        write(&path, &relay(None, Some(CA_FILE)), RelayCa::Replace(PEM)),
        remove(&path),
    ] {
        let error = refused.unwrap_err();
        assert!(
            matches!(&error, WriteError::NotAPlainFile(found) if *found == ca_path(&path)),
            "{error}"
        );
    }
    assert!(path.is_file());
    assert!(ca_path(&path).is_dir());
}

#[cfg(unix)]
#[test]
fn a_profile_file_that_is_a_link_is_left_as_it_is() {
    let folder = Folder::new("link");
    let path = folder.profile();
    let target = folder.0.join("elsewhere.toml");
    let text = "[relay]\naddress = \"relay.example.org:443\"\n";
    fs::write(&target, text).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    for refused in [
        write(&path, &relay(Some(KEY), None), RelayCa::Remove),
        remove(&path),
    ] {
        assert!(matches!(refused, Err(WriteError::NotAPlainFile(_))));
    }
    assert!(
        fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), text);
}

#[test]
fn a_folder_that_cannot_be_written_is_said_as_such_and_nothing_changes() {
    let folder = Folder::new("unwritable");
    // The folder of the program is a file here, and not there at all in the
    // second case: no system makes a new file in either, also not for an
    // administrator.
    let a_file = folder.0.join("a-file");
    fs::write(&a_file, "not a folder").unwrap();
    for path in [
        a_file.join(PROFILE_FILE),
        folder.0.join("gone").join(PROFILE_FILE),
    ] {
        for ca in [RelayCa::Remove, RelayCa::Replace(PEM)] {
            let error = write(&path, &relay(Some(KEY), None), ca).unwrap_err();
            assert!(matches!(error, WriteError::FolderNotWritable(_)), "{error}");
        }
    }
    assert_eq!(folder.names(), ["a-file"]);
}

#[test]
fn the_text_by_hand_holds_no_key() {
    let profile = RelayProfile::from_fields(
        "192.0.2.1:9970",
        Some(KEY),
        Some(CA_FILE),
        Some("relay.example.org"),
    )
    .unwrap();
    let text = text_by_hand(&profile);
    assert_eq!(
        text,
        format!(
            "[relay]\naddress = \"192.0.2.1:9970\"\n{KEY_PLACEHOLDER}\nca = \"{CA_FILE}\"\nname = \
             \"relay.example.org\"\n"
        )
    );
    assert!(!text.contains(KEY));
    // Without a key there is no line for one.
    assert!(!text_by_hand(&relay(None, None)).contains("key"));
}
