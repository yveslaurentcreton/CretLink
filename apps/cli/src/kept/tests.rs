//! Tests of the support program that a customer keeps (ST-M009-017), over a
//! computer that lies in a temporary folder: keeping, keeping again from a
//! newer copy, an older copy, the relay settings and their certificate file,
//! removing from the record, and a record that names something else.
//!
//! [`FakeSystem`] stands in for what is not a plain file. It records every
//! call, so a test can list all that keeping added and fail when anything
//! else appears.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use super::*;

/// A certificate as a PEM text, and another one.
const PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
const OTHER_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n";

/// The first line of a private key in a PEM text, written in two parts so
/// that this source holds nothing that looks like one.
const KEY_MARKER: &str = concat!("-----BEGIN PRIVATE", " KEY-----\n");

/// Records the system calls and answers from fixed state.
#[derive(Default)]
struct FakeSystem {
    /// The process runs without administrator rights.
    unelevated: Cell<bool>,
    /// The file of the program that runs.
    program: RefCell<PathBuf>,
    /// Programs that run in another process.
    running: RefCell<Vec<PathBuf>>,
    folders: RefCell<Vec<PathBuf>>,
    shortcuts: RefCell<Vec<Shortcut>>,
    entries: RefCell<BTreeMap<String, UninstallEntry>>,
    after_exit: RefCell<Vec<(Vec<PathBuf>, PathBuf)>>,
    /// How often the program that runs was held.
    held: Cell<usize>,
}

impl System for FakeSystem {
    fn require_administrator(&self) -> io::Result<()> {
        if self.unelevated.get() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(())
    }

    fn prepare_folder(&self, folder: &Path) -> io::Result<()> {
        // As off Windows: made, or found to be a folder and no link.
        match fs::create_dir(folder) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if Standing::of_dir(folder) != Standing::Ours {
                    return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                }
            }
            Err(err) => return Err(err),
        }
        self.folders.borrow_mut().push(folder.to_path_buf());
        Ok(())
    }

    fn hold_running_program(&self) -> io::Result<File> {
        self.held.set(self.held.get() + 1);
        File::open(&*self.program.borrow())
    }

    fn is_running(&self, exe: &Path) -> io::Result<bool> {
        Ok(self.running.borrow().iter().any(|path| path == exe))
    }

    fn create_shortcut(&self, shortcut: &Shortcut) -> io::Result<()> {
        // Stands in for the .lnk file, so that removing can remove it.
        fs::write(&shortcut.path, b"lnk")?;
        self.shortcuts.borrow_mut().push(shortcut.clone());
        Ok(())
    }

    fn write_uninstall_entry(&self, key: &str, entry: &UninstallEntry) -> io::Result<()> {
        self.entries
            .borrow_mut()
            .insert(key.to_owned(), entry.clone());
        Ok(())
    }

    fn has_uninstall_entry(&self, key: &str) -> bool {
        self.entries.borrow().contains_key(key)
    }

    fn remove_uninstall_entry(&self, key: &str) -> io::Result<bool> {
        Ok(self.entries.borrow_mut().remove(key).is_some())
    }

    fn remove_after_exit(&self, files: &[PathBuf], dir: &Path) -> io::Result<()> {
        self.after_exit
            .borrow_mut()
            .push((files.to_vec(), dir.to_path_buf()));
        Ok(())
    }
}

/// A computer in a temporary folder: the folder a copy was downloaded to,
/// which its account can write, and the places of a kept program.
struct Machine {
    root: PathBuf,
    /// Where the copy that runs lies.
    downloads: PathBuf,
    layout: Layout,
    system: FakeSystem,
}

impl Machine {
    fn new(test: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("cretlink-kept-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let downloads = root.join("Users").join("ann").join("Downloads");
        let program_files = root.join("Program Files");
        let start_menu = root.join("ProgramData").join("Start Menu");
        let desktop = root.join("Desktop of all users");
        for folder in [&downloads, &program_files, &start_menu, &desktop] {
            fs::create_dir_all(folder).unwrap();
        }
        let machine = Self {
            layout: Layout::new(
                program_files.join("CretLink Support"),
                &start_menu,
                &desktop,
            ),
            system: FakeSystem::default(),
            downloads,
            root,
        };
        machine.download(b"the support program, version 1");
        machine
    }

    /// The copy that runs.
    fn copy(&self) -> PathBuf {
        self.downloads.join("CretLink-Support.exe")
    }

    /// Puts a copy with `bytes` into the download folder and runs it.
    fn download(&self, bytes: &[u8]) {
        fs::write(self.copy(), bytes).unwrap();
        *self.system.program.borrow_mut() = self.copy();
    }

    fn plan(&self, version: &str) -> Result<KeepPlan, KeptError> {
        keep_plan(&self.layout, &self.system, &self.copy(), version)
    }

    /// Keeps the copy that runs as the version `version`.
    fn keep(&self, version: &str, desktop: bool) -> Result<KeepReport, KeptError> {
        keep(&self.layout, &self.system, &self.plan(version)?, desktop)
    }

    fn remove_plan(&self, current: &Path) -> RemovePlan {
        remove_plan(&self.layout, &self.system, current).unwrap()
    }

    fn remove(&self, current: &Path) -> RemoveReport {
        remove(&self.layout, &self.system, &self.remove_plan(current)).unwrap()
    }

    fn status(&self, this: &Path, version: &str) -> Status {
        status(&self.layout, this, version)
    }

    /// Every entry of this computer, as a path from its root.
    fn everything(&self) -> BTreeSet<String> {
        fn walk(dir: &Path, root: &Path, found: &mut BTreeSet<String>) {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                found.insert(name);
                if entry.file_type().unwrap().is_dir() {
                    walk(&path, root, found);
                }
            }
        }
        let mut found = BTreeSet::new();
        walk(&self.root, &self.root, &mut found);
        found
    }

    /// Writes relay settings next to the copy that runs.
    fn relay_file(&self, text: &str) {
        fs::write(self.downloads.join(PROFILE_FILE), text).unwrap();
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn names(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|path| (*path).to_owned()).collect()
}

// --- Keeping ---

#[test]
fn keeping_adds_the_program_its_entries_and_its_record_and_nothing_else() {
    let machine = Machine::new("keep");
    let layout = &machine.layout;
    let before = machine.everything();
    assert_eq!(
        machine.status(&machine.copy(), "1.0.0"),
        Status {
            relation: Relation::NotKept,
            kept_version: None
        }
    );

    let plan = machine.plan("1.0.0").unwrap();
    assert_eq!(plan.replaces, None);
    assert!(plan.relay.is_none() && plan.relay_problem.is_none());
    // Planning changes nothing.
    assert_eq!(machine.everything(), before);
    assert_eq!(machine.system.held.get(), 0);
    let report = keep(layout, &machine.system, &plan, false).unwrap();

    assert_eq!(
        report,
        KeepReport {
            exe: layout.exe.clone(),
            version: "1.0.0".to_owned(),
            replaced: None,
            relay: false,
            desktop: false,
        }
    );
    // The program that runs, byte for byte, read through the held file.
    assert_eq!(
        fs::read(&layout.exe).unwrap(),
        b"the support program, version 1"
    );
    assert_eq!(machine.system.held.get(), 1);

    // Everything that keeping added to this computer, and nothing else: no
    // other file, no second shortcut, no other entry.
    let added: BTreeSet<String> = machine.everything().difference(&before).cloned().collect();
    assert_eq!(
        added,
        names(&[
            "Program Files/CretLink Support",
            "Program Files/CretLink Support/CretLink-Support.exe",
            "Program Files/CretLink Support/install.json",
            "ProgramData/Start Menu/CretLink Support.lnk",
        ])
    );
    assert_eq!(
        *machine.system.folders.borrow(),
        vec![layout.folder.clone()]
    );
    // The Start-menu entry names the kept program by its full path, and
    // starts it without an argument: as a customer starts it.
    assert_eq!(
        *machine.system.shortcuts.borrow(),
        vec![Shortcut {
            path: layout.start_menu.clone(),
            target: layout.exe.clone(),
            arguments: String::new(),
            description: "CretLink Support: get help".to_owned(),
        }]
    );
    let exe = layout.exe.to_string_lossy().into_owned();
    let mut entries = BTreeMap::new();
    entries.insert(
        UNINSTALL_KEY.to_owned(),
        UninstallEntry {
            display_name: "CretLink Support".to_owned(),
            display_version: "1.0.0".to_owned(),
            publisher: PUBLISHER.to_owned(),
            install_location: layout.folder.to_string_lossy().into_owned(),
            uninstall_string: format!("\"{exe}\" uninstall"),
            display_icon: exe,
        },
    );
    assert_eq!(*machine.system.entries.borrow(), entries);
    assert!(machine.system.after_exit.borrow().is_empty());

    // The record names what was added.
    assert_eq!(
        read_record(layout).unwrap(),
        Some(Record {
            schema: 1,
            version: "1.0.0".to_owned(),
            files: vec![PROGRAM_FILE.to_owned(), RECORD_FILE.to_owned()],
            start_menu: Some(layout.start_menu.clone()),
            desktop: None,
            uninstall_entry: Some(format!(r"HKLM\{UNINSTALL_KEY}")),
        })
    );

    // The copy that was kept sees its own version kept; the kept program
    // sees itself.
    assert_eq!(
        machine.status(&machine.copy(), "1.0.0"),
        Status {
            relation: Relation::Same,
            kept_version: Some("1.0.0".to_owned())
        }
    );
    assert_eq!(
        machine.status(&layout.exe, "1.0.0").relation,
        Relation::ThisIsIt
    );
}

#[test]
fn a_desktop_shortcut_is_added_only_on_request_and_goes_when_it_is_no_longer_wanted() {
    let machine = Machine::new("desktop");
    let layout = &machine.layout;
    let report = machine.keep("1.0.0", true).unwrap();
    assert!(report.desktop);
    assert!(layout.desktop.is_file());
    let shortcuts = machine.system.shortcuts.borrow().clone();
    assert_eq!(shortcuts.len(), 2);
    assert!(
        shortcuts
            .iter()
            .all(|shortcut| shortcut.target == layout.exe)
    );
    assert_eq!(
        read_record(layout).unwrap().unwrap().desktop,
        Some(layout.desktop.clone())
    );

    // A newer copy that is kept says what is there, and without the wish the
    // shortcut goes again.
    machine.download(b"the support program, version 2");
    let plan = machine.plan("2.0.0").unwrap();
    assert!(plan.desktop_kept);
    keep(layout, &machine.system, &plan, false).unwrap();
    assert!(!layout.desktop.exists());
    assert_eq!(read_record(layout).unwrap().unwrap().desktop, None);
    assert!(layout.start_menu.is_file());
}

#[test]
fn a_newer_copy_updates_the_kept_program_and_names_both_versions() {
    let machine = Machine::new("update");
    let layout = &machine.layout;
    machine.keep("1.0.0", false).unwrap();
    machine.download(b"the support program, version 2");
    assert_eq!(
        machine.status(&machine.copy(), "1.1.0"),
        Status {
            relation: Relation::ThisIsNewer,
            kept_version: Some("1.0.0".to_owned())
        }
    );

    let plan = machine.plan("1.1.0").unwrap();
    assert_eq!(plan.version, "1.1.0");
    assert_eq!(plan.replaces.as_deref(), Some("1.0.0"));
    let before = machine.everything();
    let report = keep(layout, &machine.system, &plan, false).unwrap();

    assert_eq!(report.replaced.as_deref(), Some("1.0.0"));
    assert_eq!(
        fs::read(&layout.exe).unwrap(),
        b"the support program, version 2"
    );
    assert_eq!(read_record(layout).unwrap().unwrap().version, "1.1.0");
    assert_eq!(
        machine.system.entries.borrow()[UNINSTALL_KEY].display_version,
        "1.1.0"
    );
    // An update adds nothing: the same files, and no temporary one left.
    assert_eq!(machine.everything(), before);
}

#[test]
fn an_older_copy_never_replaces_a_newer_one() {
    let machine = Machine::new("older");
    let layout = &machine.layout;
    machine.keep("1.2.0", false).unwrap();
    let kept = fs::read(&layout.exe).unwrap();
    machine.download(b"an older copy");
    let before = machine.everything();

    // Older, and a preview of the release that is kept.
    for older in ["1.1.9", "0.9.0", "1.2.0-next.1"] {
        assert_eq!(
            machine.status(&machine.copy(), older).relation,
            Relation::ThisIsOlder,
            "{older}"
        );
        let error = machine.plan(older).unwrap_err();
        assert!(
            matches!(error, KeptError::NotNewer { .. }),
            "{older}: {error}"
        );
        assert_eq!(error.stage(), "keep_older");
    }
    assert_eq!(
        machine.plan("1.1.9").unwrap_err().to_string(),
        "the support program that is kept on this computer (1.2.0) is not older than this copy \
         (1.1.9); it is not replaced"
    );
    // The same version is kept already.
    assert!(matches!(
        machine.plan("1.2.0").unwrap_err(),
        KeptError::SameVersion(_)
    ));
    // A version that cannot be compared is not taken for a newer one.
    assert_eq!(
        machine.status(&machine.copy(), "a build of today").relation,
        Relation::Unknown
    );
    assert!(matches!(
        machine.plan("a build of today").unwrap_err(),
        KeptError::NotNewer { .. }
    ));
    // A record that cannot be read says nothing about the kept version.
    fs::write(&layout.record, b"not a record").unwrap();
    assert_eq!(
        machine.status(&machine.copy(), "9.0.0"),
        Status {
            relation: Relation::Unknown,
            kept_version: None
        }
    );
    assert_eq!(
        machine.plan("9.0.0").unwrap_err().to_string(),
        "a support program is kept on this computer whose version cannot be read; it is not \
         replaced by this copy (9.0.0)"
    );

    assert_eq!(fs::read(&layout.exe).unwrap(), kept);
    assert_eq!(machine.everything(), before);
    assert_eq!(
        machine.system.held.get(),
        1,
        "only the first keep held a file"
    );
}

#[test]
fn versions_are_compared_by_their_numbers() {
    use Ordering::{Equal, Greater, Less};
    for (this, kept, expected) in [
        ("1.0.0", "1.0.0", Some(Equal)),
        ("1.0.1", "1.0.0", Some(Greater)),
        ("1.10.0", "1.9.9", Some(Greater)),
        ("0.24.1", "0.25.0", Some(Less)),
        // A release is newer than a preview of it; a preview is older.
        ("1.0.0", "1.0.0-next.3", Some(Greater)),
        ("1.0.0-next.3", "1.0.0", Some(Less)),
        ("1.0.1-next.1", "1.0.0", Some(Greater)),
        ("1.0.0-next.3", "1.0.0-next.3", Some(Equal)),
        // Two previews of one release are not told apart.
        ("1.0.0-next.3", "1.0.0-next.4", None),
        // What is built beside a version does not count.
        ("1.0.0+build.7", "1.0.0", Some(Equal)),
        // Not a version.
        ("1.0", "1.0.0", None),
        ("1.0.0.0", "1.0.0", None),
        ("one", "1.0.0", None),
        ("1.0.0", "", None),
        ("1.-1.0", "1.0.0", None),
    ] {
        assert_eq!(
            compare_versions(this, kept),
            expected,
            "{this} against {kept}"
        );
    }
}

#[test]
fn the_kept_program_does_not_keep_itself_again() {
    let machine = Machine::new("itself");
    machine.keep("1.0.0", false).unwrap();
    let error = keep_plan(
        &machine.layout,
        &machine.system,
        &machine.layout.exe,
        "1.0.0",
    )
    .unwrap_err();
    assert!(matches!(error, KeptError::AlreadyKept));
    assert_eq!(error.stage(), "keep_already");
}

#[test]
fn without_administrator_rights_nothing_is_planned_kept_or_removed() {
    let machine = Machine::new("unelevated");
    machine.system.unelevated.set(true);
    let before = machine.everything();
    assert!(matches!(
        machine.plan("1.0.0").unwrap_err(),
        KeptError::NotAdministrator
    ));
    // Also not with a plan that was made with them.
    machine.system.unelevated.set(false);
    let plan = machine.plan("1.0.0").unwrap();
    machine.system.unelevated.set(true);
    let error = keep(&machine.layout, &machine.system, &plan, true).unwrap_err();
    assert_eq!(error.stage(), "keep_administrator");
    assert_eq!(machine.everything(), before);
    assert!(machine.system.entries.borrow().is_empty());

    // What is kept can be looked at without them, and not removed.
    machine.system.unelevated.set(false);
    machine.keep("1.0.0", false).unwrap();
    machine.system.unelevated.set(true);
    let kept = machine.everything();
    assert_eq!(
        machine.status(&machine.copy(), "1.0.0").relation,
        Relation::Same
    );
    let plan = machine.remove_plan(&machine.copy());
    assert!(matches!(
        remove(&machine.layout, &machine.system, &plan).unwrap_err(),
        KeptError::NotAdministrator
    ));
    assert_eq!(machine.everything(), kept);
}

#[test]
fn a_kept_program_that_runs_is_neither_replaced_nor_removed() {
    let machine = Machine::new("running");
    let layout = &machine.layout;
    machine.keep("1.0.0", false).unwrap();
    machine.download(b"the support program, version 2");
    machine.system.running.borrow_mut().push(layout.exe.clone());
    let before = machine.everything();

    let error = machine.keep("2.0.0", false).unwrap_err();
    assert!(matches!(error, KeptError::Running(_)), "{error}");
    assert_eq!(error.stage(), "keep_running");
    let plan = machine.remove_plan(&machine.copy());
    assert!(matches!(
        remove(layout, &machine.system, &plan).unwrap_err(),
        KeptError::Running(_)
    ));

    assert_eq!(
        fs::read(&layout.exe).unwrap(),
        b"the support program, version 1"
    );
    assert_eq!(machine.everything(), before);
    assert!(machine.system.entries.borrow().contains_key(UNINSTALL_KEY));
}

#[test]
fn nothing_is_written_through_what_is_no_file_or_no_folder() {
    // A folder stands where the program belongs.
    let machine = Machine::new("foreign");
    let layout = &machine.layout;
    fs::create_dir_all(layout.exe.join("inside")).unwrap();
    let error = machine.keep("1.0.0", false).unwrap_err();
    assert_eq!(error.stage(), "keep_failed");
    assert!(error.to_string().starts_with("could not write the program"));
    assert!(layout.exe.join("inside").is_dir(), "it is left as it was");
    // No shortcut and no entry were added for a program that is not there.
    assert!(machine.system.shortcuts.borrow().is_empty());
    assert!(machine.system.entries.borrow().is_empty());

    // A file stands where the folder belongs.
    let machine = Machine::new("foreign-folder");
    fs::write(&machine.layout.folder, b"theirs").unwrap();
    let error = machine.keep("1.0.0", false).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("could not make the folder of the kept program"),
        "{error}"
    );
    assert_eq!(fs::read(&machine.layout.folder).unwrap(), b"theirs");
}

#[cfg(unix)]
#[test]
fn a_link_under_a_name_of_the_kept_program_is_never_followed() {
    use std::os::unix::fs::symlink;

    // A link where the program belongs: nothing is written behind it.
    let machine = Machine::new("link");
    let layout = &machine.layout;
    let theirs = machine.root.join("theirs.exe");
    fs::write(&theirs, b"theirs").unwrap();
    fs::create_dir(&layout.folder).unwrap();
    symlink(&theirs, &layout.exe).unwrap();
    assert_eq!(
        machine.status(&machine.copy(), "1.0.0").relation,
        Relation::NotKept,
        "a link is not a kept program"
    );
    assert!(machine.keep("1.0.0", false).is_err());
    assert_eq!(fs::read(&theirs).unwrap(), b"theirs");

    // A link where a shortcut belongs.
    let machine = Machine::new("link-shortcut");
    let theirs = machine.root.join("theirs.lnk");
    fs::write(&theirs, b"theirs").unwrap();
    symlink(&theirs, &machine.layout.start_menu).unwrap();
    let error = machine.keep("1.0.0", false).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("could not add the Start-menu entry"),
        "{error}"
    );
    assert_eq!(fs::read(&theirs).unwrap(), b"theirs");
    assert!(machine.system.shortcuts.borrow().is_empty());

    // A link where the folder belongs: nothing is made behind it.
    let machine = Machine::new("link-folder");
    let elsewhere = machine.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, &machine.layout.folder).unwrap();
    assert!(machine.keep("1.0.0", false).is_err());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn what_was_written_is_read_back_against_the_program_that_runs() {
    let machine = Machine::new("read-back");
    let target = machine.root.join("written");
    let source = File::open(machine.copy()).unwrap();
    write_program(&source, &target).unwrap();
    assert_eq!(
        fs::read(&target).unwrap(),
        b"the support program, version 1"
    );

    // A file that does not hold what was meant never takes the name.
    let other = machine.root.join("other");
    let error = write_new(
        &other,
        &mut |file| file.write_all(b"something else"),
        &mut |written| {
            let mut source = &source;
            source.seek(SeekFrom::Start(0))?;
            if same_bytes(&mut source, written)? {
                Ok(())
            } else {
                Err(io::Error::other("not the same"))
            }
        },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "not the same");
    assert!(!other.exists());
    assert!(!temporary_name(&other).exists(), "nothing is left of it");

    // The same bytes, a byte less, a byte more, another byte.
    let same = |one: &[u8], other: &[u8]| {
        let (mut one, mut other) = (one, other);
        same_bytes(&mut one, &mut other).unwrap()
    };
    assert!(same(b"", b""));
    assert!(same(b"abc", b"abc"));
    assert!(!same(b"abc", b"ab"));
    assert!(!same(b"ab", b"abc"));
    assert!(!same(b"abc", b"abd"));
    let long = vec![7u8; 200_000];
    let mut other = long.clone();
    assert!(same(&long, &other));
    other[150_000] = 8;
    assert!(!same(&long, &other));
}

// --- The relay settings that come with the program ---

#[test]
fn the_relay_settings_of_the_copy_are_kept_as_they_were_read() {
    let machine = Machine::new("relay");
    let layout = &machine.layout;
    // The settings as a technician hands them out, with the file of an
    // authority as some tools write it: its private key first, then its
    // certificate.
    machine.relay_file(
        "# for Ann\n[relay]\naddress = \"192.0.2.1:9970\"\nkey = \"key-of-ann\"\nca = \
         \"authority.pem\"\nname = \"relay.example.org\"\n",
    );
    let end = KEY_MARKER.replace("BEGIN", "END");
    let whole = format!(
        "# kept by Sam\n{KEY_MARKER}bm90IGEga2V5\n{end}{}",
        String::from_utf8_lossy(PEM)
    );
    fs::write(machine.downloads.join("authority.pem"), whole).unwrap();

    let plan = machine.plan("1.0.0").unwrap();
    let relay = plan.relay.as_ref().unwrap();
    // What the customer is shown before choosing.
    assert_eq!(relay.profile().server_name(), "relay.example.org");
    assert_eq!(relay.profile().address().to_string(), "192.0.2.1:9970");
    assert!(relay.has_certificate_file());
    assert!(!plan.replaces_relay);

    // The files next to the copy change after they were shown: what is kept
    // is what was shown.
    machine.relay_file("[relay]\naddress = \"host-01.example.org:443\"\n");
    fs::write(machine.downloads.join("authority.pem"), OTHER_PEM).unwrap();
    let report = keep(layout, &machine.system, &plan, false).unwrap();
    assert!(report.relay);

    let kept = relay_profile::read_file(&layout.profile).unwrap().unwrap();
    assert_eq!(kept.address().to_string(), "192.0.2.1:9970");
    assert_eq!(kept.name(), Some("relay.example.org"));
    assert_eq!(
        kept.key().map(|key| key.as_bytes().to_vec()),
        Some(b"key-of-ann".to_vec())
    );
    // The certificate file under the name the kept program has for it, with
    // the certificates only: the key of the authority is not in it.
    assert_eq!(kept.ca(), Some(RELAY_CA_FILE));
    assert_eq!(fs::read(&layout.relay_ca).unwrap(), PEM);
    assert_eq!(
        relay_profile::read_ca(&layout.profile, &kept).unwrap(),
        Some(PEM.to_vec())
    );
    assert!(!layout.folder.join("authority.pem").exists());
    assert_eq!(
        read_record(layout).unwrap().unwrap().files,
        vec![
            PROGRAM_FILE.to_owned(),
            RECORD_FILE.to_owned(),
            PROFILE_FILE.to_owned(),
            RELAY_CA_FILE.to_owned()
        ]
    );
}

#[test]
fn the_relay_settings_of_a_newer_copy_replace_the_kept_ones() {
    let machine = Machine::new("relay-update");
    let layout = &machine.layout;
    machine.relay_file("[relay]\naddress = \"192.0.2.1:9970\"\nca = \"authority.pem\"\n");
    fs::write(machine.downloads.join("authority.pem"), PEM).unwrap();
    machine.keep("1.0.0", false).unwrap();
    assert!(layout.relay_ca.is_file());

    // A newer copy with other settings, whose relay has no certificate file.
    machine.download(b"the support program, version 2");
    machine.relay_file("[relay]\naddress = \"host-01.example.org:443\"\n");
    let plan = machine.plan("2.0.0").unwrap();
    assert!(
        plan.replaces_relay,
        "the screen says that they are replaced"
    );
    keep(layout, &machine.system, &plan, false).unwrap();
    let kept = relay_profile::read_file(&layout.profile).unwrap().unwrap();
    assert_eq!(kept.address().to_string(), "host-01.example.org:443");
    assert_eq!(kept.ca(), None);
    assert!(
        !layout.relay_ca.exists(),
        "the certificate file of before went"
    );

    // A newer copy without settings keeps the ones that are there.
    machine.download(b"the support program, version 3");
    fs::remove_file(machine.downloads.join(PROFILE_FILE)).unwrap();
    let plan = machine.plan("3.0.0").unwrap();
    assert!(plan.relay.is_none() && !plan.replaces_relay);
    let report = keep(layout, &machine.system, &plan, false).unwrap();
    assert!(report.relay);
    let kept = relay_profile::read_file(&layout.profile).unwrap().unwrap();
    assert_eq!(kept.address().to_string(), "host-01.example.org:443");
    // And they are still named by the record, so that removing takes them.
    assert!(
        read_record(layout)
            .unwrap()
            .unwrap()
            .files
            .contains(&PROFILE_FILE.to_owned())
    );
}

#[test]
fn relay_settings_that_cannot_be_used_are_not_kept_and_are_said() {
    for (case, text, certificate, expected) in [
        (
            "unknown",
            "[relay]\naddress = \"192.0.2.1:9970\"\nsecret = \"key-of-ann\"\n",
            None,
            "line 3: this is not a setting of a relay; a relay has `address`, `key`, `ca` and \
             `name`",
        ),
        (
            "no-certificate-file",
            "[relay]\naddress = \"192.0.2.1:9970\"\nca = \"authority.pem\"\n",
            None,
            "`ca` names a file that is not in the folder of this file",
        ),
        (
            "no-certificate",
            "[relay]\naddress = \"192.0.2.1:9970\"\nca = \"authority.pem\"\n",
            Some(KEY_MARKER.as_bytes()),
            "the certificate file holds no certificate that can be read; a PEM file has blocks \
             between -----BEGIN CERTIFICATE----- and -----END CERTIFICATE-----",
        ),
    ] {
        let machine = Machine::new(&format!("relay-{case}"));
        machine.relay_file(text);
        if let Some(certificate) = certificate {
            fs::write(machine.downloads.join("authority.pem"), certificate).unwrap();
        }

        let plan = machine.plan("1.0.0").unwrap();

        assert!(plan.relay.is_none(), "{case}");
        let problem = plan.relay_problem.as_ref().unwrap();
        assert_eq!(problem.file, machine.downloads.join(PROFILE_FILE), "{case}");
        assert_eq!(problem.problem.to_string(), expected, "{case}");
        // Nothing of what the file held is repeated.
        assert!(!problem.to_string().contains("key-of-ann"), "{case}");
        // The program is kept without them.
        let report = keep(&machine.layout, &machine.system, &plan, false).unwrap();
        assert!(!report.relay, "{case}");
        assert!(!machine.layout.profile.exists(), "{case}");
        assert!(!machine.layout.relay_ca.exists(), "{case}");
    }
}

#[test]
fn a_file_that_is_too_large_or_no_file_is_not_read() {
    let machine = Machine::new("relay-bounds");
    // A folder under the name of the settings.
    fs::create_dir(machine.downloads.join(PROFILE_FILE)).unwrap();
    let problem = read_relay_next_to(&machine.copy()).unwrap_err();
    assert_eq!(
        problem.problem.to_string(),
        "the file could not be read (invalid input parameter)"
    );
    fs::remove_dir(machine.downloads.join(PROFILE_FILE)).unwrap();

    // Larger than relay settings are.
    let large = format!(
        "[relay]\naddress = \"192.0.2.1:9970\"\n{}",
        "# filler\n".repeat(2000)
    );
    assert!(large.len() as u64 > MAX_PROFILE_BYTES);
    machine.relay_file(&large);
    let problem = read_relay_next_to(&machine.copy()).unwrap_err();
    assert_eq!(
        problem.problem.to_string(),
        "the file could not be read (invalid data)"
    );

    // A file without relay settings, and no file: nothing to keep.
    machine.relay_file("# nothing yet\n");
    assert!(read_relay_next_to(&machine.copy()).unwrap().is_none());
    fs::remove_file(machine.downloads.join(PROFILE_FILE)).unwrap();
    assert!(read_relay_next_to(&machine.copy()).unwrap().is_none());

    // The bound itself: one byte below it is read, one above is not.
    let path = machine.root.join("bounded");
    fs::write(&path, vec![b'x'; 16]).unwrap();
    assert_eq!(read_untrusted(&path, 16).unwrap().unwrap().len(), 16);
    assert!(read_untrusted(&path, 15).is_err());
}

#[cfg(unix)]
#[test]
fn relay_settings_behind_a_link_are_not_read() {
    use std::os::unix::fs::symlink;

    let machine = Machine::new("relay-link");
    // The settings of somebody else, somewhere else.
    let theirs = machine.root.join("theirs.toml");
    fs::write(&theirs, "[relay]\naddress = \"192.0.2.1:9970\"\n").unwrap();
    symlink(&theirs, machine.downloads.join(PROFILE_FILE)).unwrap();

    let plan = machine.plan("1.0.0").unwrap();

    assert!(plan.relay.is_none());
    assert_eq!(
        plan.relay_problem.unwrap().problem.to_string(),
        "the file could not be read (invalid input parameter)"
    );

    // The same for a certificate file behind a link.
    fs::remove_file(machine.downloads.join(PROFILE_FILE)).unwrap();
    machine.relay_file("[relay]\naddress = \"192.0.2.1:9970\"\nca = \"authority.pem\"\n");
    let authority = machine.root.join("authority.pem");
    fs::write(&authority, PEM).unwrap();
    symlink(&authority, machine.downloads.join("authority.pem")).unwrap();
    let plan = machine.plan("1.0.0").unwrap();
    assert!(plan.relay.is_none());
    assert_eq!(
        plan.relay_problem.unwrap().problem.to_string(),
        "the file that `ca` names could not be read (invalid input parameter)"
    );
}

// --- Removing ---

#[test]
fn removing_takes_what_the_record_names_and_leaves_nothing() {
    let machine = Machine::new("remove");
    let layout = &machine.layout;
    let before = machine.everything();
    machine.relay_file("[relay]\naddress = \"192.0.2.1:9970\"\nca = \"authority.pem\"\n");
    fs::write(machine.downloads.join("authority.pem"), PEM).unwrap();
    let with_copy = machine.everything();
    machine.keep("1.0.0", true).unwrap();

    // Shown before anything changes; from another copy, at once.
    let plan = machine.remove_plan(&machine.copy());
    assert!(!plan.after_exit);
    assert!(plan.left.is_empty(), "{:?}", plan.left);
    assert_eq!(
        plan.lines(),
        vec![
            layout.exe.display().to_string(),
            layout.profile.display().to_string(),
            layout.relay_ca.display().to_string(),
            layout.record.display().to_string(),
            format!("The folder {}", layout.folder.display()),
            format!("The shortcut {}", layout.start_menu.display()),
            format!("The shortcut {}", layout.desktop.display()),
            format!(r"The entry in Installed apps (HKLM\{UNINSTALL_KEY})"),
        ]
    );
    assert!(layout.exe.exists());

    let report = remove(layout, &machine.system, &plan).unwrap();

    assert!(report.complete(), "{report:?}");
    assert!(report.pending.is_empty() && report.left.is_empty());
    // Nothing stays behind: the computer is as it was with the copy alone.
    assert_eq!(machine.everything(), with_copy);
    assert!(machine.system.entries.borrow().is_empty());
    assert_ne!(
        with_copy, before,
        "the copy and its settings are not touched"
    );
    // A second look finds nothing of it.
    assert!(matches!(
        remove_plan(layout, &machine.system, &machine.copy()).unwrap_err(),
        KeptError::NotKept(_)
    ));
    assert_eq!(
        machine.status(&machine.copy(), "1.0.0").relation,
        Relation::NotKept
    );
}

#[test]
fn the_kept_program_removes_its_folder_once_it_has_exited() {
    let machine = Machine::new("remove-itself");
    let layout = &machine.layout;
    machine.keep("1.0.0", false).unwrap();

    let plan = machine.remove_plan(&layout.exe);
    assert!(plan.after_exit);
    assert!(plan.lines().contains(&format!(
        "{} (removed once CretLink has closed)",
        layout.exe.display()
    )));
    let report = remove(layout, &machine.system, &plan).unwrap();

    assert!(report.complete(), "{report:?}");
    assert_eq!(
        report.pending,
        vec![format!(
            "The folder {}, once CretLink has closed",
            layout.folder.display()
        )]
    );
    // The shortcut and the entry are gone at once; the program that runs is
    // left to the helper, with its folder.
    assert!(!layout.start_menu.exists());
    assert!(machine.system.entries.borrow().is_empty());
    assert!(layout.exe.exists());
    assert_eq!(
        *machine.system.after_exit.borrow(),
        vec![(
            vec![layout.exe.clone(), layout.record.clone()],
            layout.folder.clone()
        )]
    );
}

#[test]
fn a_record_that_names_something_else_removes_nothing_outside_the_layout() {
    let machine = Machine::new("tampered");
    let layout = &machine.layout;
    machine.keep("1.0.0", false).unwrap();
    // Files that are nobody's to remove: beside the folder, above it, on the
    // desktop of an account, and one in the folder itself.
    let beside = layout.folder.with_file_name("victim.txt");
    let desktop = machine.root.join("Users").join("ann").join("victim.lnk");
    for victim in [&beside, &desktop] {
        fs::write(victim, b"keep").unwrap();
    }
    let mut record = read_record(layout).unwrap().unwrap();
    record.files.extend([
        "../victim.txt".to_owned(),
        beside.to_string_lossy().into_owned(),
        r"..\victim.txt".to_owned(),
        "C:victim.txt".to_owned(),
        "..".to_owned(),
        String::new(),
    ]);
    record.start_menu = Some(desktop.clone());
    record.desktop = Some(beside.clone());
    record.uninstall_entry = Some(r"HKLM\Software\Other".to_owned());
    write_record(layout, &record).unwrap();

    let plan = machine.remove_plan(&machine.copy());

    // Only what lies in the layout is planned, and the rest is said.
    assert_eq!(plan.files, vec![layout.exe.clone(), layout.record.clone()]);
    assert!(plan.shortcuts.is_empty(), "{:?}", plan.shortcuts);
    assert_eq!(plan.uninstall_entry.as_deref(), Some(UNINSTALL_KEY));
    assert_eq!(
        plan.left,
        vec![
            format!(
                "An entry of the record that does not name a file in {}",
                layout.folder.display()
            );
            6
        ]
        .into_iter()
        .chain(vec![
            "A shortcut that the record names outside its own place"
                .to_owned();
            2
        ])
        .collect::<Vec<_>>()
    );
    let report = remove(layout, &machine.system, &plan).unwrap();

    assert!(report.complete(), "{report:?}");
    for victim in [&beside, &desktop] {
        assert_eq!(fs::read(victim).unwrap(), b"keep", "{}", victim.display());
    }
    assert!(!layout.folder.exists());
    // The Start-menu entry that the record no longer names stays, and so
    // does everything of the folders above.
    assert!(layout.start_menu.exists());
    assert!(layout.folder.parent().unwrap().is_dir());
}

#[test]
fn what_cretlink_did_not_put_into_the_folder_keeps_it() {
    let machine = Machine::new("foreign-file");
    let layout = &machine.layout;
    machine.keep("1.0.0", false).unwrap();
    let theirs = layout.folder.join("notes.txt");
    fs::write(&theirs, b"theirs").unwrap();

    let plan = machine.remove_plan(&machine.copy());
    assert_eq!(
        plan.left,
        vec![format!(
            "The folder {} with 1 entry in it that CretLink did not put there",
            layout.folder.display()
        )]
    );
    let report = remove(layout, &machine.system, &plan).unwrap();

    assert!(report.complete(), "{report:?}");
    assert_eq!(report.left, plan.left);
    assert!(!layout.exe.exists() && !layout.record.exists());
    assert_eq!(fs::read(&theirs).unwrap(), b"theirs");
}

#[test]
fn without_a_record_only_cretlinks_own_names_are_removed() {
    let machine = Machine::new("no-record");
    let layout = &machine.layout;
    machine.keep("1.0.0", true).unwrap();
    // The record is gone, and a keep that was interrupted left a file under
    // a temporary name.
    fs::remove_file(&layout.record).unwrap();
    fs::write(temporary_name(&layout.exe), b"half").unwrap();

    let plan = machine.remove_plan(&machine.copy());
    assert_eq!(
        plan.files,
        vec![temporary_name(&layout.exe), layout.exe.clone()]
    );
    assert_eq!(
        plan.shortcuts,
        vec![layout.start_menu.clone(), layout.desktop.clone()]
    );
    let report = remove(layout, &machine.system, &plan).unwrap();
    assert!(report.complete(), "{report:?}");
    assert!(!layout.folder.exists());

    // A record that cannot be read is said, and removed like any file.
    let machine = Machine::new("bad-record");
    let layout = &machine.layout;
    machine.keep("1.0.0", false).unwrap();
    fs::write(&layout.record, b"{ not a record").unwrap();
    let plan = machine.remove_plan(&machine.copy());
    assert_eq!(plan.left.len(), 1, "{:?}", plan.left);
    assert!(
        plan.left[0].starts_with("What only the record could name, because "),
        "{:?}",
        plan.left
    );
    assert!(remove(layout, &machine.system, &plan).unwrap().complete());
    assert!(!layout.folder.exists());
}

#[cfg(unix)]
#[test]
fn removing_never_follows_a_link() {
    use std::os::unix::fs::symlink;

    // A link under the name of a file of the kept program.
    let machine = Machine::new("remove-link");
    let layout = &machine.layout;
    machine.keep("1.0.0", false).unwrap();
    let theirs = machine.root.join("theirs.toml");
    fs::write(&theirs, b"theirs").unwrap();
    symlink(&theirs, &layout.profile).unwrap();
    let mut record = read_record(layout).unwrap().unwrap();
    record.files.push(PROFILE_FILE.to_owned());
    write_record(layout, &record).unwrap();
    assert!(machine.remove(&machine.copy()).complete());
    assert_eq!(fs::read(&theirs).unwrap(), b"theirs");
    assert!(!layout.folder.exists());

    // A link in the place of the folder: nothing behind it is touched.
    let machine = Machine::new("remove-link-folder");
    let layout = &machine.layout;
    machine.keep("1.0.0", false).unwrap();
    let moved = machine.root.join("moved");
    fs::rename(&layout.folder, &moved).unwrap();
    symlink(&moved, &layout.folder).unwrap();
    let plan = machine.remove_plan(&machine.copy());
    assert!(plan.files.is_empty(), "{:?}", plan.files);
    assert_eq!(plan.left.len(), 1);
    assert!(remove(layout, &machine.system, &plan).unwrap().complete());
    assert!(moved.join(PROGRAM_FILE).exists());
    assert!(moved.join(RECORD_FILE).exists());
}

#[test]
fn nothing_kept_is_nothing_to_remove() {
    let machine = Machine::new("not-kept");
    let error = remove_plan(&machine.layout, &machine.system, &machine.copy()).unwrap_err();
    assert!(matches!(error, KeptError::NotKept(_)));
    assert_eq!(
        error.to_string(),
        format!(
            "CretLink Support is not kept on this computer (nothing of it in {})",
            machine.layout.folder.display()
        )
    );
}

#[test]
fn the_plan_and_the_report_of_a_removal_say_what_goes_and_what_stays() {
    let plan = RemovePlan {
        folder: PathBuf::from("/c/Program Files/CretLink Support"),
        files: vec![PathBuf::from(
            "/c/Program Files/CretLink Support/CretLink-Support.exe",
        )],
        shortcuts: vec![PathBuf::from("/c/Start Menu/CretLink Support.lnk")],
        uninstall_entry: Some(UNINSTALL_KEY.to_owned()),
        after_exit: false,
        left: vec!["A shortcut that the record names outside its own place".to_owned()],
    };
    assert_eq!(
        remove_plan_text(&plan),
        "Remove CretLink Support from this computer (/c/Program Files/CretLink Support).\n\
         This removes:\n\
         \x20 - /c/Program Files/CretLink Support/CretLink-Support.exe\n\
         \x20 - The folder /c/Program Files/CretLink Support\n\
         \x20 - The shortcut /c/Start Menu/CretLink Support.lnk\n\
         \x20 - The entry in Installed apps \
         (HKLM\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\CretLink Support)\n\
         This stays:\n\
         \x20 - A shortcut that the record names outside its own place\n\
         \x20 - A session report that you saved, where you saved it\n"
    );
    let done = RemoveReport {
        removed: vec!["The entry in Installed apps".to_owned()],
        pending: vec!["The folder x, once CretLink has closed".to_owned()],
        ..RemoveReport::default()
    };
    assert_eq!(
        remove_report_text(&done),
        "Removed:\n  - The entry in Installed apps\nRemoved once CretLink has closed:\n  - The \
         folder x, once CretLink has closed\nCretLink Support was removed from this computer.\n"
    );
    let failed = RemoveReport {
        failed: vec!["x: denied".to_owned()],
        ..RemoveReport::default()
    };
    let text = remove_report_text(&failed);
    assert!(
        text.starts_with("Could not remove:\n  - x: denied\n"),
        "{text}"
    );
    assert!(text.contains("was not completely removed"), "{text}");
}

#[test]
fn the_layout_names_every_part_of_a_kept_program() {
    let layout = Layout::new(
        PathBuf::from("/c/Program Files/CretLink Support"),
        Path::new("/c/ProgramData/Start Menu/Programs"),
        Path::new("/c/Desktop of all users"),
    );
    assert_eq!(
        layout.exe,
        Path::new("/c/Program Files/CretLink Support/CretLink-Support.exe")
    );
    assert_eq!(
        layout.record,
        Path::new("/c/Program Files/CretLink Support/install.json")
    );
    assert_eq!(
        layout.profile,
        Path::new("/c/Program Files/CretLink Support/cretlink.toml")
    );
    assert_eq!(
        layout.relay_ca,
        Path::new("/c/Program Files/CretLink Support/relay-ca.pem")
    );
    assert_eq!(
        layout.start_menu,
        Path::new("/c/ProgramData/Start Menu/Programs/CretLink Support.lnk")
    );
    assert_eq!(
        layout.desktop,
        Path::new("/c/Desktop of all users/CretLink Support.lnk")
    );
    assert_eq!(layout.uninstall_key, UNINSTALL_KEY);
    // The kept program keeps the name that opens the customer role only.
    assert!(crate::install::is_customer_copy(&layout.exe));
}

/// Off Windows the customer role does not exist: no layout, and a system
/// that does nothing.
#[cfg(not(windows))]
#[test]
fn off_windows_nothing_is_kept() {
    assert_eq!(Layout::current(), None);
    let system = OsSystem;
    assert_eq!(
        system.require_administrator().unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert!(system.hold_running_program().is_err());
    assert!(!system.has_uninstall_entry(UNINSTALL_KEY));
    let machine = Machine::new("off-windows");
    assert!(matches!(
        keep_plan(&machine.layout, &system, &machine.copy(), "1.0.0").unwrap_err(),
        KeptError::NotAdministrator
    ));
    assert_eq!(
        uninstall_command("CretLink-Support", false, false),
        ExitCode::SUCCESS
    );
}

/// The real system, in a sandbox: the folder is made with its permissions,
/// the program that runs is held and written, the shortcuts are real, and
/// the entry lies in the registry under a key of this test. Everything that
/// was added is listed, and removing leaves nothing.
#[cfg(windows)]
#[test]
fn on_windows_the_real_system_keeps_and_removes_in_a_sandbox() {
    let system = OsSystem;
    if system.require_administrator().is_err() {
        assert!(
            std::env::var_os("CI").is_none(),
            "continuous verification runs as an administrator"
        );
        eprintln!("skipped: this process is not an administrator");
        return;
    }
    let machine = Machine::new("real");
    let mut layout = machine.layout.clone();
    layout.uninstall_key = format!("{UNINSTALL_KEY} test {}", std::process::id());
    let this = std::env::current_exe().unwrap();
    let before = machine.everything();

    let plan = keep_plan(&layout, &system, &this, "9.9.9").unwrap();
    let report = keep(&layout, &system, &plan, true).unwrap();

    assert!(report.desktop && !report.relay);
    // The program that runs, byte for byte.
    assert_eq!(fs::read(&layout.exe).unwrap(), fs::read(&this).unwrap());
    let added: BTreeSet<String> = machine.everything().difference(&before).cloned().collect();
    assert_eq!(
        added,
        names(&[
            "Program Files/CretLink Support",
            "Program Files/CretLink Support/CretLink-Support.exe",
            "Program Files/CretLink Support/install.json",
            "ProgramData/Start Menu/CretLink Support.lnk",
            "Desktop of all users/CretLink Support.lnk",
        ])
    );
    assert!(system.has_uninstall_entry(&layout.uninstall_key));
    assert_eq!(
        status(&layout, &this, "9.9.9"),
        Status {
            relation: Relation::Same,
            kept_version: Some("9.9.9".to_owned())
        }
    );

    let plan = remove_plan(&layout, &system, &this).unwrap();
    assert!(!plan.after_exit && plan.left.is_empty(), "{plan:?}");
    let report = remove(&layout, &system, &plan).unwrap();

    assert!(report.complete(), "{report:?}");
    assert_eq!(machine.everything(), before);
    assert!(!system.has_uninstall_entry(&layout.uninstall_key));
}
