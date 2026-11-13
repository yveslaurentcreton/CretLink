//! Make a customer copy (ST-M007-016, ADR-0016 §4, ADR-0018).
//!
//! A technician hands a customer one folder, and the customer needs nothing
//! else: the support program, and the file that tells it which relay to
//! connect through.
//!
//! ```text
//! CretLink-Support-<label>/
//!   CretLink-Support.exe   the program, copied unchanged
//!   cretlink.toml          the relay's address, this customer's key, and the
//!                          name and certificate file when the relay has them
//!   relay-ca.pem           the certificates of the relay's own authority,
//!                          under the name the profile gives the file, when
//!                          it names one
//!   README.txt             a few lines for the customer
//! ```
//!
//! | Command | What it does |
//! |---|---|
//! | [`customer_copy_options`] | which relay a copy would name, and where its program comes from |
//! | [`choose_customer_program`] | the system's dialog for `cretlink.exe` of a release (macOS, Linux) |
//! | [`choose_customer_place`] | the system's dialog for the folder the copy is made in |
//! | [`generate_customer_key`] | a new random admission key, for the window to show once |
//! | [`make_customer_copy`] | writes the folder |
//! | [`show_customer_copy`] | shows the folder that was written in the file manager |
//!
//! # What holds
//!
//! - **The technician's own key is never written.** Nothing here reads the
//!   key of the technician's profile. A copy holds the key that the
//!   technician typed or had generated for this customer, or none.
//! - **A generated key comes from the system's random source**
//!   (`cretlink_relay::generate_key`) and is handed to the window once, to
//!   show the line `label:key` for the relay's key file (ST-M007-014).
//!   Nothing here keeps it.
//! - **Every name that goes into a path is one plain name**, checked before
//!   anything is written: the label, the folder made from it and the name of
//!   the certificate file. The copy is for a Windows computer, so a name is
//!   also one that Windows takes, wherever the copy is made.
//! - **The program is copied unchanged**, byte for byte: a signed program
//!   stays signed.
//! - **No half copy that looks whole.** The folder is written beside its
//!   place under another name and takes its name only when it is complete
//!   and was read back. A failure removes what was written, or says what
//!   could not be removed and where it lies.
//! - **Read back before success.** The written `cretlink.toml` is read with
//!   the reader of the customer role itself (ST-M007-011), with its
//!   certificate file, and must say what was meant.
//! - **An existing folder is never replaced without asking.** The first
//!   attempt answers that the folder exists; only the technician's answer
//!   lets it be replaced.
//! - **Nothing leaves this computer.** Files are written, and a folder is
//!   shown in the file manager. No connection is opened.
//! - **A key grants nothing but the use of the relay** (ADR-0016 §4). It
//!   stands in `cretlink.toml` in readable form, admits a copy to the
//!   forwarder and gives no control over a session; the relay's operator
//!   withdraws it by deleting its line from the key file.

use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cretlink_cli::relay_profile::{self, Field, PROFILE_FILE, RelayProfile};
use serde::{Deserialize, Serialize};
use tauri::{State, WebviewWindow};

use super::{AppState, CommandError, relay_settings, setup};
use crate::relay::{Lookup, Relay, RelayView, next_to_program};

/// The name of the support program in a customer copy. A copy of the
/// application under a name that starts with `CretLink-Support` opens in the
/// customer role.
pub(crate) const PROGRAM_NAME: &str = "CretLink-Support.exe";

/// The name of the note for the customer.
pub(crate) const README_NAME: &str = "README.txt";

/// What the name of the folder of a customer copy begins with; the label
/// follows.
pub(crate) const FOLDER_PREFIX: &str = "CretLink-Support-";

/// The longest label.
pub(crate) const MAX_LABEL_CHARS: usize = 40;

/// What follows the folder's name while the copy is written beside its
/// place, and while a folder that is replaced is set aside.
const NEW_SUFFIX: &str = ".cretlink-new";
const OLD_SUFFIX: &str = ".cretlink-old";

/// What a customer copy is made from.
#[derive(Clone, Copy)]
pub(crate) struct Plan<'a> {
    /// The relay this technician copy uses. Its address, the name in its
    /// certificate and its certificate authority go into the customer copy.
    /// Its key does not.
    pub(crate) relay: &'a Relay,
    /// Who the copy is for: letters, digits and hyphens.
    pub(crate) label: &'a str,
    /// The admission key of this customer, as the technician typed it or had
    /// it generated; `None` for a relay that asks for no key.
    pub(crate) key: Option<&'a str>,
    /// The program that is copied as [`PROGRAM_NAME`].
    pub(crate) program: &'a Path,
    /// The folder the technician chose; the copy is made inside it.
    pub(crate) place: &'a Path,
    /// Whether a folder of the copy's name that is there already may be
    /// replaced: the technician was asked and said so.
    pub(crate) replace: bool,
}

/// The key is not printed.
impl fmt::Debug for Plan<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plan")
            .field("relay", &self.relay.host())
            .field("label", &self.label)
            .field("has_key", &self.key.is_some())
            .field("program", &self.program)
            .field("place", &self.place)
            .field("replace", &self.replace)
            .finish()
    }
}

/// A customer copy that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Made {
    /// The folder of the copy.
    pub(crate) folder: PathBuf,
    /// The names of its files, in the order a person reads them.
    pub(crate) files: Vec<String>,
    /// The folder that was replaced, when it was set aside and could not be
    /// removed afterwards. The copy itself is whole.
    pub(crate) set_aside: Option<PathBuf>,
}

/// Why no customer copy was made. Nothing of the copy is left on disk, with
/// one exception that [`CopyError::Write`] names itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CopyError {
    /// The label is not letters, digits and hyphens within the bound.
    Label(String),
    /// The key cannot be a key of a relay profile.
    Key(String),
    /// The program to copy is not there or is no Windows program.
    Program(String),
    /// The place to make the folder in cannot be used.
    Place(String),
    /// A folder of the copy's name is there already. Nothing was changed;
    /// the technician is asked whether to replace it.
    Exists(PathBuf),
    /// The relay of this copy cannot be put into a customer copy.
    Relay(String),
    /// Writing failed. `left` is what was written and could not be removed
    /// again, when there is such a thing.
    Write {
        /// What failed.
        message: String,
        /// What was written and is still there.
        left: Option<PathBuf>,
    },
}

impl CopyError {
    /// The name of the failure for the window: the field it is about, or
    /// what to ask.
    pub(crate) const fn stage(&self) -> &'static str {
        match self {
            Self::Label(_) => "copy_label",
            Self::Key(_) => "copy_key",
            Self::Program(_) => "copy_program",
            Self::Place(_) => "copy_place",
            Self::Exists(_) => "copy_exists",
            Self::Relay(_) => "copy_relay",
            Self::Write { .. } => "copy_write",
        }
    }
}

/// What happened, as a sentence. It never holds the key.
impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Label(text)
            | Self::Key(text)
            | Self::Program(text)
            | Self::Place(text)
            | Self::Relay(text) => f.write_str(text),
            Self::Exists(folder) => write!(
                f,
                "A folder {} is there already. Nothing was changed.",
                folder.display()
            ),
            Self::Write {
                message,
                left: None,
            } => write!(
                f,
                "The customer copy could not be written: {message}. Nothing of it was left \
                 behind."
            ),
            Self::Write {
                message,
                left: Some(left),
            } => write!(
                f,
                "The customer copy could not be written: {message}. What was written could not \
                 be removed again and lies in {}; it is not a copy that can be used, so remove \
                 it yourself.",
                left.display()
            ),
        }
    }
}

impl From<&CopyError> for CommandError {
    fn from(error: &CopyError) -> Self {
        Self::new(error.stage(), error)
    }
}

/// Whether `label` can name a customer copy: letters, digits and hyphens, at
/// least one and at most [`MAX_LABEL_CHARS`].
///
/// # Errors
///
/// Returns what is asked of a label, in words that do not repeat it.
pub(crate) fn check_label(label: &str) -> Result<(), CopyError> {
    let fits = !label.is_empty()
        && label.len() <= MAX_LABEL_CHARS
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if fits {
        Ok(())
    } else {
        Err(CopyError::Label(format!(
            "The label holds letters, digits and hyphens only, at least one and at most \
             {MAX_LABEL_CHARS}. It becomes part of the folder's name."
        )))
    }
}

/// The name of the folder of the customer copy `label`.
pub(crate) fn folder_name(label: &str) -> String {
    format!("{FOLDER_PREFIX}{label}")
}

/// Whether `name` is one plain file name that a Windows computer takes: no
/// folder in it, none of the characters and device names Windows refuses,
/// and nothing that Windows would change (a blank or a dot at its end).
///
/// A customer copy is unpacked on Windows, wherever it was made, so the rule
/// is the same on every system.
pub(crate) fn is_plain_windows_name(name: &str) -> bool {
    const REFUSED: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    const DEVICES: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if name.is_empty() || name.chars().count() > relay_profile::MAX_CA_NAME_CHARS {
        return false;
    }
    if name == "." || name == ".." || name.ends_with('.') || name.trim() != name {
        return false;
    }
    if name.chars().any(|c| c.is_control() || REFUSED.contains(&c)) {
        return false;
    }
    // `NUL`, `nul.txt` and `Nul.tar.gz` all name the device.
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    !DEVICES
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(device))
}

/// The text of the `cretlink.toml` of a customer copy.
fn profile_text(label: &str, profile: &RelayProfile) -> String {
    format!(
        "# The relay of this CretLink support program, for {label}.\n\
         # Keep this file next to {PROGRAM_NAME}.\n\
         # A key in it stands in readable form. It only lets this copy use the relay.\n\
         {}",
        profile.to_section().as_str()
    )
}

/// The note for the customer. Windows line ends: the customer opens it there.
fn readme_text() -> String {
    [
        "CretLink support program",
        "",
        "Your technician prepared this folder for you.",
        "",
        "1. Keep the files of this folder together.",
        "2. When your technician asks you to, start CretLink-Support.exe.",
        "   Windows asks whether the program may make changes to this computer.",
        "   That is expected: say yes only while you are talking to your",
        "   technician.",
        "3. Type the code your technician reads to you.",
        "4. Compare the six digits on your screen with the ones your technician",
        "   reads back. Allow access only when they are the same.",
        "",
        "Nothing is installed. Your technician can see and control this computer",
        "only while the program is open and only after you allowed it. You see",
        "what they do, you can stop at any time, and closing the program ends",
        "everything.",
        "",
        "The file cretlink.toml tells the program which relay to connect through",
        "when you and your technician are not on the same network. A key in it",
        "only lets this copy use that relay. It gives nobody access to this",
        "computer.",
        "",
    ]
    .join("\r\n")
}

/// Whether the file `program` begins as a Windows program does.
fn is_windows_program(program: &Path) -> io::Result<bool> {
    let mut start = [0u8; 2];
    let mut file = fs::File::open(program)?;
    match file.read_exact(&mut start) {
        Ok(()) => Ok(&start == b"MZ"),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

/// Why a file could not be read or written, in words that hold nothing of
/// the system's own message, which may name paths.
fn io_reason(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "this account may not write there",
        io::ErrorKind::NotFound => "a file or folder is no longer there",
        io::ErrorKind::AlreadyExists => "something of that name is there already",
        io::ErrorKind::StorageFull => "the disk is full",
        _ => "the system refused",
    }
}

/// Checks the program that would be copied.
fn check_program(program: &Path) -> Result<(), CopyError> {
    let refused = |text: &str| CopyError::Program(text.to_owned());
    let is_file = fs::metadata(program).is_ok_and(|meta| meta.is_file());
    if !is_file {
        return Err(refused(
            "The program to copy is not there. Choose cretlink.exe from the Windows archive of a \
             CretLink release.",
        ));
    }
    match is_windows_program(program) {
        Ok(true) => Ok(()),
        Ok(false) => Err(refused(
            "That file is not a Windows program. A customer copy holds cretlink.exe from the \
             Windows archive of a CretLink release.",
        )),
        Err(_) => Err(refused("The program to copy could not be read.")),
    }
}

/// Removes a folder or file that this module made under one of its own
/// names. Nothing to remove is fine.
fn remove_own(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

/// Writes a new file. `private` keeps it for this user where the system has
/// file modes: it holds a key.
fn write_new(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    options.open(path)?.write_all(bytes)
}

/// A customer copy that is written and read back, beside the place it will
/// take: the folder exists under another name and is whole. [`Self::place`]
/// gives it its name; dropping it removes it.
#[derive(Debug)]
pub(crate) struct Staged {
    /// The folder as it is now, under its temporary name.
    stage: PathBuf,
    /// The folder as it will be.
    target: PathBuf,
    files: Vec<String>,
    replace: bool,
    placed: bool,
}

impl Staged {
    /// The folder under its temporary name, for a test that looks at it.
    #[cfg(test)]
    pub(crate) fn stage(&self) -> &Path {
        &self.stage
    }

    /// Gives the folder its name. A folder of that name is replaced only
    /// when the plan said so; it is set aside first and removed last, so
    /// that it is back in its place when the new one cannot take it.
    ///
    /// # Errors
    ///
    /// [`CopyError::Exists`] when a folder of that name is there and may not
    /// be replaced, and [`CopyError::Write`] when the folder could not take
    /// its place. What was written is removed either way.
    pub(crate) fn place(mut self) -> Result<Made, CopyError> {
        let old = self.target.with_file_name(format!(
            "{}{OLD_SUFFIX}",
            self.target
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));
        let there = fs::symlink_metadata(&self.target).is_ok();
        if there && !self.replace {
            // What was written goes; the folder that is there stays.
            return Err(
                match self.give_up("a folder of that name is there already") {
                    CopyError::Write { left: None, .. } => CopyError::Exists(self.target.clone()),
                    left_behind => left_behind,
                },
            );
        }
        let moved = (|| {
            if there {
                remove_own(&old)?;
                fs::rename(&self.target, &old)?;
            }
            fs::rename(&self.stage, &self.target).inspect_err(|_| {
                // The folder of before goes back to where it was.
                if there {
                    let _ = fs::rename(&old, &self.target);
                }
            })
        })();
        if let Err(error) = moved {
            return Err(self.give_up(&format!(
                "the folder could not take its place ({})",
                io_reason(&error)
            )));
        }
        self.placed = true;
        let set_aside = (there && remove_own(&old).is_err()).then_some(old);
        Ok(Made {
            folder: self.target.clone(),
            files: std::mem::take(&mut self.files),
            set_aside,
        })
    }

    /// Removes what was written and answers the failure `message`, with what
    /// was left when it could not be removed.
    fn give_up(&mut self, message: &str) -> CopyError {
        let left = match remove_own(&self.stage) {
            Ok(()) => None,
            Err(_) => Some(self.stage.clone()),
        };
        // Whatever could be removed is gone; there is nothing more to try.
        self.placed = true;
        CopyError::Write {
            message: message.to_owned(),
            left,
        }
    }
}

/// A copy that did not take its place is removed: nothing that looks like a
/// customer copy stays behind under the temporary name.
impl Drop for Staged {
    fn drop(&mut self) {
        if !self.placed {
            let _ = remove_own(&self.stage);
        }
    }
}

/// Checks a plan and writes the customer copy beside its place, then reads
/// it back. Nothing has the copy's own name yet ([`Staged::place`]).
///
/// # Errors
///
/// Returns why no copy can be made. Everything that can be said before
/// writing is checked before anything is written; a failure after that
/// removes what was written.
pub(crate) fn stage(plan: Plan<'_>) -> Result<Staged, CopyError> {
    // --- What can be said before anything is written ---
    check_label(plan.label)?;
    let own = plan.relay.profile();
    // A form that says there is a key and brings none.
    if plan.key == Some("") {
        return Err(CopyError::Key(
            "Type the admission key of this customer or generate one, or say that the relay asks \
             for no key."
                .to_owned(),
        ));
    }
    // The customer's profile: the relay of this copy with the customer's
    // key in the place of the technician's, which is not read.
    let profile =
        RelayProfile::from_fields(&own.address().to_string(), plan.key, own.ca(), own.name())
            .map_err(|problem| match problem.field {
                // The profile's words name the setting of a file; here it is
                // a field of a form. They never repeat the key.
                Some(Field::Key) => CopyError::Key(match problem.text.strip_prefix("`key` ") {
                    Some(what) => format!("The key {what}."),
                    None => "The key cannot be used.".to_owned(),
                }),
                _ => CopyError::Relay(format!(
                    "The relay of this copy cannot be written into a customer copy: {}.",
                    problem.text
                )),
            })?;
    let ca_name = profile.ca().map(str::to_owned);
    if let Some(name) = &ca_name {
        let taken = [PROGRAM_NAME, PROFILE_FILE, README_NAME]
            .iter()
            .any(|own| own.eq_ignore_ascii_case(name));
        if taken || !is_plain_windows_name(name) {
            return Err(CopyError::Relay(
                "The certificate file of this copy's relay has a name that a customer copy \
                 cannot hold. Give the file a plain name such as relay-ca.pem."
                    .to_owned(),
            ));
        }
    }
    let certificates = relay_profile::read_ca(plan.relay.source().path(), own)
        .and_then(|pem| {
            pem.map(|pem| {
                relay_profile::certificates_only(&pem).map_err(|problem| {
                    relay_profile::ProfileError {
                        file: plan.relay.source().path().to_path_buf(),
                        problem,
                    }
                })
            })
            .transpose()
        })
        .map_err(|error| {
            CopyError::Relay(format!(
                "The certificate file of this copy's relay cannot be read: {}.",
                error.problem.text
            ))
        })?;
    check_program(plan.program)?;
    if !plan.place.is_absolute() || !plan.place.is_dir() {
        return Err(CopyError::Place(
            "The folder to make the copy in is not there. Choose it again.".to_owned(),
        ));
    }
    let name = folder_name(plan.label);
    let target = plan.place.join(&name);
    match fs::symlink_metadata(&target) {
        Err(_) => {}
        Ok(_) if !plan.replace => return Err(CopyError::Exists(target)),
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(CopyError::Place(format!(
                "Something that is no folder has the name {name} there. Choose another label or \
                 another place."
            )));
        }
    }

    // --- Writing, beside the place the folder will take ---
    let stage = plan.place.join(format!("{name}{NEW_SUFFIX}"));
    let unwritable = |error: &io::Error| CopyError::Write {
        message: io_reason(error).to_owned(),
        left: None,
    };
    // One left behind by a run that was interrupted.
    remove_own(&stage).map_err(|error| unwritable(&error))?;
    fs::create_dir(&stage).map_err(|error| unwritable(&error))?;
    // From here on the folder is removed when this function leaves early.
    let mut staged = Staged {
        stage: stage.clone(),
        target,
        files: Vec::new(),
        replace: plan.replace,
        placed: false,
    };
    let written = (|| -> io::Result<()> {
        let program = stage.join(PROGRAM_NAME);
        let copied = fs::copy(plan.program, &program)?;
        if copied != fs::metadata(plan.program)?.len() {
            return Err(io::Error::other("the program changed while it was copied"));
        }
        staged.files.push(PROGRAM_NAME.to_owned());
        write_new(
            &stage.join(PROFILE_FILE),
            profile_text(plan.label, &profile).as_bytes(),
            true,
        )?;
        staged.files.push(PROFILE_FILE.to_owned());
        if let (Some(name), Some(certificates)) = (&ca_name, &certificates) {
            write_new(&stage.join(name), certificates, false)?;
            staged.files.push(name.clone());
        }
        write_new(&stage.join(README_NAME), readme_text().as_bytes(), false)?;
        staged.files.push(README_NAME.to_owned());
        Ok(())
    })();
    if let Err(error) = written {
        return Err(staged.give_up(io_reason(&error)));
    }

    // --- Read back, as the customer's copy will read it ---
    let program = stage.join(PROGRAM_NAME);
    let said = match relay_profile::read_next_to(&program) {
        Ok(Some((_, read))) => *read.to_section() == *profile.to_section(),
        Ok(None) | Err(_) => false,
    };
    let usable = match next_to_program(&program) {
        Lookup::Relay(read) => read.route() == plan.relay.route(),
        Lookup::None | Lookup::Unusable(_) => false,
    };
    if !said || !usable {
        return Err(staged.give_up("what was written did not read back as the relay of this copy"));
    }
    Ok(staged)
}

/// Makes the customer copy of `plan`: checks it, writes it beside its place,
/// reads it back and gives it its name.
///
/// # Errors
///
/// Returns why no copy was made ([`CopyError`]).
pub(crate) fn make(plan: Plan<'_>) -> Result<Made, CopyError> {
    stage(plan)?.place()
}

// --- What the window and the commands share ---

/// Where the program of a customer copy comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramSource {
    /// This program itself, on Windows: it is copied under another name.
    ThisProgram,
    /// `cretlink.exe` from the Windows archive of a release, which the
    /// technician points at (macOS, Linux).
    Choose,
}

impl ProgramSource {
    /// Where the program comes from on this system.
    pub(crate) const fn of_this_system() -> Self {
        if cfg!(windows) {
            Self::ThisProgram
        } else {
            Self::Choose
        }
    }
}

/// What **Make a customer copy…** shows before anything is typed
/// (`CustomerCopyOptions` of the contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerCopyOptions {
    /// The relay a copy would name. Its key is no part of it.
    pub relay: RelayView,
    /// Where the program comes from.
    pub program: ProgramSource,
    /// The longest label.
    pub label_max: usize,
    /// The names the folder and its program get.
    pub folder_prefix: &'static str,
    /// The name of the program in the folder.
    pub program_name: &'static str,
}

impl CustomerCopyOptions {
    pub(crate) fn of(relay: &Relay) -> Self {
        Self {
            // What a copy would hold: the customer's key is not known yet,
            // and the technician's is not part of a copy.
            relay: RelayView {
                key_set: false,
                ..relay.view()
            },
            program: ProgramSource::of_this_system(),
            label_max: MAX_LABEL_CHARS,
            folder_prefix: FOLDER_PREFIX,
            program_name: PROGRAM_NAME,
        }
    }
}

/// Something the technician chose in a dialog of the system (`ChosenForCopy`
/// of the contract): the number a form names it by, and its name or path for
/// the window's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Chosen {
    /// The number of this choice.
    pub pick: u64,
    /// The file's name, or the folder's path.
    pub name: String,
}

/// A new admission key (`GeneratedKey` of the contract), for the window to
/// show once with the label as the line of the relay's key file.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GeneratedKey {
    /// The key.
    pub key: String,
}

/// The key is not printed.
impl fmt::Debug for GeneratedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedKey").finish_non_exhaustive()
    }
}

/// What to do about the admission key of the customer copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyKeyMode {
    /// The key of the form: typed for this customer, or generated.
    Set,
    /// No key: the relay asks for none.
    None,
}

/// The customer copy as the form gives it (`CustomerCopyForm` of the
/// contract). There is no way to ask for the technician's own key.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CustomerCopyForm {
    /// Who the copy is for.
    pub label: String,
    /// What to do about the key.
    pub key_mode: CopyKeyMode,
    /// The key, with [`CopyKeyMode::Set`].
    #[serde(default)]
    pub key: Option<String>,
    /// The number of the chosen program, where the technician chooses one.
    #[serde(default)]
    pub program_pick: Option<u64>,
    /// The number of the chosen place.
    #[serde(default)]
    pub place_pick: Option<u64>,
    /// Whether a folder of that name may be replaced: the technician was
    /// asked.
    #[serde(default)]
    pub replace: bool,
}

/// The key is not printed.
impl fmt::Debug for CustomerCopyForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CustomerCopyForm")
            .field("label", &self.label)
            .field("key_mode", &self.key_mode)
            .field("has_key", &self.key.is_some())
            .field("program_pick", &self.program_pick)
            .field("place_pick", &self.place_pick)
            .field("replace", &self.replace)
            .finish()
    }
}

/// A customer copy that was written (`CustomerCopyMade` of the contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerCopyMade {
    /// The number **Show in folder** names the copy by.
    pub copy: u64,
    /// The folder, for the window's words.
    pub folder: String,
    /// The names of the files in it.
    pub files: Vec<String>,
    /// The folder that was replaced, when it could not be removed and still
    /// lies beside the copy under another name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_aside: Option<String>,
}

/// What the technician chose for a copy and what was made, kept on this
/// side: the window names a program, a place and a copy by a number and
/// never sends a path of this computer.
#[derive(Debug, Clone, Default)]
pub(crate) struct Picks(Arc<Mutex<Kept>>);

#[derive(Debug, Default)]
struct Kept {
    program: Option<(u64, PathBuf)>,
    place: Option<(u64, PathBuf)>,
    made: Option<(u64, PathBuf)>,
}

/// The number of the next choice or copy.
static NEXT_PICK: AtomicU64 = AtomicU64::new(1);

fn next_pick() -> u64 {
    NEXT_PICK.fetch_add(1, Ordering::Relaxed)
}

impl Picks {
    fn kept(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps the program the technician chose and answers its number.
    pub(crate) fn keep_program(&self, program: PathBuf) -> u64 {
        let pick = next_pick();
        self.kept().program = Some((pick, program));
        pick
    }

    /// Keeps the place the technician chose and answers its number.
    pub(crate) fn keep_place(&self, place: PathBuf) -> u64 {
        let pick = next_pick();
        self.kept().place = Some((pick, place));
        pick
    }

    fn program(&self, pick: Option<u64>) -> Option<PathBuf> {
        let kept = self.kept();
        let (number, path) = kept.program.as_ref()?;
        (Some(*number) == pick).then(|| path.clone())
    }

    fn place(&self, pick: Option<u64>) -> Option<PathBuf> {
        let kept = self.kept();
        let (number, path) = kept.place.as_ref()?;
        (Some(*number) == pick).then(|| path.clone())
    }

    fn keep_made(&self, folder: PathBuf) -> u64 {
        let pick = next_pick();
        self.kept().made = Some((pick, folder));
        pick
    }

    /// The folder of the copy `copy`, while it is the one made last.
    pub(crate) fn made(&self, copy: u64) -> Option<PathBuf> {
        let kept = self.kept();
        let (number, path) = kept.made.as_ref()?;
        (*number == copy).then(|| path.clone())
    }
}

/// Makes the customer copy of `form`, with the relay `relay` of this
/// technician copy and `this_program` as the program on a system where the
/// copy is this program itself. Answers what was written.
pub(crate) fn make_from_form(
    relay: &Relay,
    form: &CustomerCopyForm,
    picks: &Picks,
    source: ProgramSource,
    this_program: &Path,
) -> Result<CustomerCopyMade, CopyError> {
    // The label first: it is the first thing on the form.
    check_label(&form.label)?;
    let key = match form.key_mode {
        CopyKeyMode::Set => Some(form.key.as_deref().unwrap_or_default()),
        CopyKeyMode::None => None,
    };
    let program = match source {
        ProgramSource::ThisProgram => this_program.to_path_buf(),
        ProgramSource::Choose => picks.program(form.program_pick).ok_or_else(|| {
            CopyError::Program(
                "Choose cretlink.exe from the Windows archive of a CretLink release.".to_owned(),
            )
        })?,
    };
    let place = picks
        .place(form.place_pick)
        .ok_or_else(|| CopyError::Place("Choose the folder to make the copy in.".to_owned()))?;
    let made = make(Plan {
        relay,
        label: &form.label,
        key,
        program: &program,
        place: &place,
        replace: form.replace,
    })?;
    Ok(CustomerCopyMade {
        copy: picks.keep_made(made.folder.clone()),
        folder: made.folder.display().to_string(),
        files: made.files,
        set_aside: made.set_aside.map(|path| path.display().to_string()),
    })
}

// --- Commands ---

/// Which relay a customer copy would name, and where its program comes from.
/// Refuses when this copy has no relay that can be used.
#[tauri::command]
pub(crate) async fn customer_copy_options() -> Result<CustomerCopyOptions, CommandError> {
    setup::blocking(|| Ok(CustomerCopyOptions::of(&relay_settings::relay_in_use()?))).await
}

/// Opens the system's dialog for `cretlink.exe` from the Windows archive of
/// a release, and keeps the file that was chosen. Answers `null` when the
/// dialog was closed without a choice.
#[tauri::command]
pub(crate) async fn choose_customer_program(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<Chosen>, CommandError> {
    let Some(file) = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .set_title("cretlink.exe from the Windows archive of a CretLink release")
        .add_filter("Windows programs", &["exe"])
        .add_filter("All files", &["*"])
        .pick_file()
        .await
    else {
        return Ok(None);
    };
    let path = file.path().to_path_buf();
    let name = file.file_name();
    let checked = path.clone();
    setup::blocking(move || check_program(&checked).map_err(|error| (&error).into())).await?;
    Ok(Some(Chosen {
        pick: state.customer_copy.keep_program(path),
        name,
    }))
}

/// Opens the system's dialog for the folder the customer copy is made in,
/// and keeps the folder that was chosen. Answers `null` when the dialog was
/// closed without a choice.
#[tauri::command]
pub(crate) async fn choose_customer_place(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<Chosen>, CommandError> {
    let Some(folder) = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .set_title("Make the customer copy in this folder")
        .pick_folder()
        .await
    else {
        return Ok(None);
    };
    let path = folder.path().to_path_buf();
    Ok(Some(Chosen {
        name: path.display().to_string(),
        pick: state.customer_copy.keep_place(path),
    }))
}

/// Draws a new admission key from the system's random source, for the
/// window to show once as the line of the relay's key file. Nothing is kept
/// here.
#[tauri::command]
pub(crate) fn generate_customer_key() -> Result<GeneratedKey, CommandError> {
    cretlink_relay::generate_key()
        .map(|key| GeneratedKey { key })
        .map_err(|_| {
            CommandError::new(
                "copy_key",
                "This computer gave no randomness for a key. Try again.",
            )
        })
}

/// Writes the customer copy of the form.
#[tauri::command]
pub(crate) async fn make_customer_copy(
    state: State<'_, AppState>,
    form: CustomerCopyForm,
) -> Result<CustomerCopyMade, CommandError> {
    let picks = state.customer_copy.clone();
    setup::blocking(move || {
        let relay = relay_settings::relay_in_use()?;
        let this_program = std::env::current_exe()
            .map_err(|_| CommandError::new("current_exe", "CretLink cannot find itself."))?;
        make_from_form(
            &relay,
            &form,
            &picks,
            ProgramSource::of_this_system(),
            &this_program,
        )
        .map_err(|error| (&error).into())
    })
    .await
}

/// Shows the folder of the customer copy `copy` in the file manager. Only a
/// folder that this process wrote can be shown.
#[tauri::command]
pub(crate) async fn show_customer_copy(
    state: State<'_, AppState>,
    copy: u64,
) -> Result<(), CommandError> {
    let folder = state.customer_copy.made(copy).ok_or_else(|| {
        CommandError::new("copy_show", "That customer copy is no longer at hand.")
    })?;
    setup::blocking(move || {
        crate::opener::show_folder(&folder).map_err(|error| {
            CommandError::new(
                "copy_show",
                format!("The folder could not be shown: {error}."),
            )
        })
    })
    .await
}

#[cfg(test)]
mod tests;
