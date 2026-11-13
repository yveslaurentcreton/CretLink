//! The settings of the installed copy (ST-M007-011).
//!
//! The installed technician copy keeps two things between runs: the relay
//! profile that **Settings → Relay** edits ([`crate::relay_profile`]), and the
//! network adapter the technician chose to wait on (ST-M009-016). Both stand
//! in one file next to the install record ([`Layout::settings`]):
//!
//! ```toml
//! [relay]
//! address = "relay.example.org:443"
//! key = "5f2c9d0e…"
//! ca = "relay-ca.pem"
//!
//! [network]
//! adapter = "Wi-Fi"
//! ```
//!
//! When the relay's certificate comes from a private authority, a copy of the
//! certificates of that authority's PEM file stands next to it as
//! [`Layout::relay_ca`], and the profile names it. The copy holds the
//! certificates only: a private key in the chosen file is not kept. The two
//! files change together ([`write_with_relay_ca`]): a change that fails
//! leaves both as they were.
//!
//! - **Only the installed copy** reads and writes these files. A copy that
//!   runs once writes nothing but its audit trail, and the customer role
//!   reads only the profile file next to its program; neither calls this
//!   module's writers.
//! - **For this user only.** The file holds the relay's admission key in
//!   readable form. Where the system has file modes (macOS, Linux) it is
//!   created with mode `0600`: read and write for the user, nothing for
//!   anybody else. On Windows it inherits the access rules of the user's own
//!   `%LOCALAPPDATA%` folder (the user, the system and administrators); the
//!   application sets none of its own there.
//! - **Removed by uninstall.** `cretlink uninstall` removes both files with
//!   the rest of the install ([`crate::install`]).
//! - **Read like any file that somebody else may have written**: bounded, in
//!   the small form of [`crate::relay_profile`], with every unknown section
//!   or setting refused, and with messages that never repeat what the file
//!   held.

use std::fs;
use std::io::{self, Write as _};
use std::path::Path;

use zeroize::Zeroizing;

use crate::install::{Layout, RELAY_CA_FILE, remove_file_if_exists, temporary_name};
use crate::relay_profile::{
    self, Field, MAX_CA_BYTES, MAX_PROFILE_BYTES, Problem, ProfileError, RELAY_SECTION,
    RelayProfile, document,
};

/// The section that holds the network to wait on.
pub const NETWORK_SECTION: &str = "network";

/// The setting of [`NETWORK_SECTION`] that names the adapter.
const ADAPTER: &str = "adapter";

/// The longest name of a network adapter that is kept.
pub const MAX_ADAPTER_CHARS: usize = 256;

/// The settings a message about the settings file may name: those of a
/// relay, and the adapter.
fn known_names() -> Vec<document::Known> {
    let mut known = relay_profile::KNOWN_FIELDS.to_vec();
    known.push(document::Known {
        name: ADAPTER,
        field: None,
    });
    known
}

/// What the installed copy keeps between runs.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The relay this copy uses, unless a profile file next to the program
    /// names another.
    pub relay: Option<RelayProfile>,
    /// The network adapter to wait on, by the name the operating system gives
    /// it (ST-M009-016).
    pub network_adapter: Option<String>,
}

impl Settings {
    /// Whether there is nothing to keep.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.relay.is_none() && self.network_adapter.is_none()
    }

    /// The text of the settings file. It holds the relay's key in readable
    /// form and is erased when dropped.
    fn to_text(&self) -> Zeroizing<String> {
        let mut out = Zeroizing::new(String::from(
            "# Settings of the installed CretLink. Written by CretLink; change them in Settings.\n",
        ));
        if let Some(relay) = &self.relay {
            out.push('\n');
            out.push_str(&relay.to_section());
        }
        if let Some(adapter) = &self.network_adapter {
            out.push_str(&format!("\n[{NETWORK_SECTION}]\n{ADAPTER} = "));
            out.push_str(&document::quoted(adapter));
            out.push('\n');
        }
        out
    }
}

/// Whether `adapter` is a name that is kept: not empty, bounded, one line.
#[must_use]
pub fn is_adapter_name(adapter: &str) -> bool {
    !adapter.trim().is_empty()
        && adapter.chars().count() <= MAX_ADAPTER_CHARS
        && !adapter.chars().any(char::is_control)
}

/// Reads the text of a settings file.
///
/// # Errors
///
/// Returns what is wrong, with the line. An unknown section or setting is an
/// error, and so is a relay whose certificate file is not the one this
/// module writes.
pub fn parse(text: &str) -> Result<Settings, Problem> {
    let document = document::parse(text, &known_names())?;
    let mut settings = Settings::default();
    for section in &document.sections {
        match section.name.as_str() {
            RELAY_SECTION => {
                let relay = relay_profile::from_section(section)?;
                if relay.ca().is_some_and(|ca| ca != RELAY_CA_FILE) {
                    return Err(Problem {
                        line: Some(section.line),
                        field: Some(Field::Ca),
                        text: format!(
                            "the certificate file of the relay in the settings is {RELAY_CA_FILE}; \
                             choose the file again in Settings"
                        ),
                    });
                }
                settings.relay = Some(relay);
            }
            NETWORK_SECTION => {
                for setting in &section.settings {
                    if setting.name != ADAPTER {
                        return Err(Problem::at(
                            setting.line,
                            "this is not a setting of the network; it has `adapter`",
                        ));
                    }
                    if !is_adapter_name(&setting.value) {
                        return Err(Problem::at(
                            setting.line,
                            "`adapter` is the name of a network adapter, on one line",
                        ));
                    }
                    settings.network_adapter = Some(setting.value.to_string());
                }
            }
            _ => {
                return Err(Problem::at(
                    section.line,
                    "the settings have the sections [relay] and [network]",
                ));
            }
        }
    }
    Ok(settings)
}

/// Reads the settings of the installed copy. A file that is not there holds
/// nothing.
///
/// # Errors
///
/// Returns the file and what is wrong with it.
pub fn read(layout: &Layout) -> Result<Settings, ProfileError> {
    let in_file = |problem: Problem| ProfileError {
        file: layout.settings.clone(),
        problem,
    };
    let Some(bytes) =
        relay_profile::read_bounded(&layout.settings, MAX_PROFILE_BYTES, "the settings file")
            .map_err(in_file)?
    else {
        return Ok(Settings::default());
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| in_file(Problem::file("the settings file is not text (UTF-8)")))?;
    parse(text).map_err(in_file)
}

/// Writes the settings of the installed copy, for this user only, replacing
/// the file in one step. Settings that hold nothing remove the file. The
/// certificate authority file of the relay is not touched: a change of the
/// relay goes through [`write_with_relay_ca`].
///
/// # Errors
///
/// Returns the error of the file system, or `InvalidInput` for an adapter
/// name that cannot be kept. The file that was there is then unchanged.
pub fn write(layout: &Layout, settings: &Settings) -> io::Result<()> {
    if let Some(adapter) = &settings.network_adapter
        && !is_adapter_name(adapter)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the name of the network adapter cannot be kept",
        ));
    }
    if settings.is_empty() {
        return remove_file_if_exists(&layout.settings).map(drop);
    }
    write_private(&layout.settings, settings.to_text().as_bytes())
}

/// What a change of the relay in the settings does to the certificate
/// authority file that is kept next to them ([`Layout::relay_ca`], which the
/// profile names as [`RELAY_CA_FILE`]).
#[derive(Debug, Clone, Copy)]
pub enum RelayCa<'a> {
    /// The file stays as it is: the relay keeps its authority, or has none
    /// and there is no file.
    Keep,
    /// No file: the relay is checked against the system's authorities, or
    /// the settings hold no relay any more.
    Remove,
    /// The certificates of this PEM text become the file. Only the
    /// certificates are kept, written anew
    /// ([`relay_profile::certificates_only`]): a private key that stood in
    /// the same file is not.
    Replace(&'a [u8]),
}

/// Writes the settings and the certificate authority file of their relay as
/// one change: afterwards both are as asked, or both are as they were.
///
/// The two are one thing to a person: a relay and the authority its
/// certificate is checked against. A change that reached only one of them
/// would leave the relay of before with the authority of after, or the other
/// way round. So the new certificate file is written under another name
/// first, the settings are written, and only then the certificate file takes
/// its place or goes; when that last step fails, the settings of before are
/// put back.
///
/// A crash between the two steps leaves the new settings with the
/// certificate file of before. That fails closed: the new relay is then
/// checked against the old authority, and nothing is sent to a relay whose
/// certificate that authority did not sign.
///
/// # Errors
///
/// Returns `InvalidInput` when a certificate text holds no certificate or is
/// larger than a certificate authority file may be, and for an adapter name
/// that cannot be kept; otherwise the error of the file system. Nothing was
/// changed then, with one exception that the error says itself
/// ([`is_not_put_back`]): the last step failed and the settings of before
/// could not be written again.
pub fn write_with_relay_ca(
    layout: &Layout,
    settings: &Settings,
    ca: RelayCa<'_>,
) -> io::Result<()> {
    let invalid = |text: &str| io::Error::new(io::ErrorKind::InvalidInput, text.to_owned());
    // The certificate file of after, under another name: the one in use is
    // not touched until the settings are written.
    if let RelayCa::Replace(pem) = ca {
        let certificates = relay_profile::certificates_only(pem)
            .map_err(|_| invalid("the certificate file holds no certificate"))?;
        if certificates.len() as u64 > MAX_CA_BYTES {
            return Err(invalid("the certificate file is too large"));
        }
        stage_private(&layout.relay_ca, &certificates)?;
    }
    let discard = || {
        if matches!(ca, RelayCa::Replace(_)) {
            let _ = fs::remove_file(temporary_name(&layout.relay_ca));
        }
    };
    // What the settings hold now, to put it back. A file that cannot be read
    // cannot be put back either: then nothing is begun.
    let before = match fs::read(&layout.settings) {
        Ok(bytes) => Some(Zeroizing::new(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            discard();
            return Err(error);
        }
    };
    if let Err(error) = write(layout, settings) {
        discard();
        return Err(error);
    }
    let moved = match ca {
        RelayCa::Keep => Ok(()),
        RelayCa::Remove => remove_file_if_exists(&layout.relay_ca).map(drop),
        RelayCa::Replace(_) => fs::rename(temporary_name(&layout.relay_ca), &layout.relay_ca),
    };
    let Err(error) = moved else {
        return Ok(());
    };
    discard();
    let restored = match &before {
        Some(bytes) => write_private(&layout.settings, bytes),
        None => remove_file_if_exists(&layout.settings).map(drop),
    };
    Err(match restored {
        Ok(()) => error,
        Err(restore) => io::Error::other(NotPutBack {
            change: error,
            restore,
        }),
    })
}

/// A change of the settings that failed half way and could not be undone:
/// the settings are the new ones, and the certificate file is still the one
/// of before. It is the payload of the error of [`write_with_relay_ca`] in
/// that one case, so that a caller never says "nothing was changed" then
/// ([`is_not_put_back`]).
#[derive(Debug)]
pub struct NotPutBack {
    /// Why the certificate file could not take its place or go.
    pub change: io::Error,
    /// Why the settings of before could not be written again.
    pub restore: io::Error,
}

impl std::fmt::Display for NotPutBack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the certificate file could not be changed ({}), and the settings of before could \
             not be put back ({}); the relay in the settings is the new one and its certificate \
             file is the old one, so save or remove the relay again",
            self.change, self.restore
        )
    }
}

impl std::error::Error for NotPutBack {}

/// Whether `error` of [`write_with_relay_ca`] says that the settings were
/// changed and could not be put back. Every other error of it means that
/// nothing was changed.
#[must_use]
pub fn is_not_put_back(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<NotPutBack>())
}

/// Writes `bytes` under the temporary name of `path`, in the same folder and
/// for this user only. One left behind by a run that was interrupted is
/// replaced. `path` itself is not touched.
fn stage_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_name(path);
    remove_file_if_exists(&temporary)?;
    let written = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // Read and write for the user, nothing for anybody else, from
            // the moment the file exists.
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Writes `bytes` to `path` through a temporary file in the same folder that
/// only this user can read, so that the file is never there half written and
/// never readable by others on the way.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    stage_private(path, bytes)?;
    let temporary = temporary_name(path);
    let moved = fs::rename(&temporary, path);
    if moved.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{BaseDirs, Platform};
    use std::path::PathBuf;

    const KEY: &str = "5f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5";

    /// A certificate file in the form it is kept in. Its one block is no
    /// real certificate: whether a certificate can be used is decided where
    /// the TLS settings are built.
    const PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

    /// The first line of a private key in the text form of a key file,
    /// written in two parts so that no scanner takes this file for one that
    /// holds a key.
    const KEY_MARKER: &str = concat!("-----BEGIN PRIVATE", " KEY-----\n");

    /// A home folder of its own for one test, removed afterwards.
    struct Home {
        root: PathBuf,
        layout: Layout,
    }

    impl Home {
        fn new(test: &str, platform: Platform) -> Self {
            let root = std::env::temp_dir()
                .join(format!("cretlink-settings-{test}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let home = root.join("home");
            let dirs = BaseDirs {
                home: home.clone(),
                local_app_data: Some(home.join("AppData").join("Local")),
                app_data: Some(home.join("AppData").join("Roaming")),
                data_home: None,
            };
            let layout = Layout::new(platform, &dirs).unwrap();
            Self { root, layout }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn relay() -> RelayProfile {
        RelayProfile::from_fields(
            "relay.example.org:443",
            Some(KEY),
            Some(RELAY_CA_FILE),
            Some("host-01.example.org"),
        )
        .unwrap()
    }

    fn key_of(settings: &Settings) -> Option<String> {
        settings
            .relay
            .as_ref()
            .and_then(RelayProfile::key)
            .map(|key| String::from_utf8_lossy(key.as_bytes()).into_owned())
    }

    #[test]
    fn the_settings_stand_next_to_the_install_record_on_every_system() {
        for platform in [Platform::Windows, Platform::MacOs, Platform::Linux] {
            let home = Home::new("where", platform);
            let layout = &home.layout;
            assert_eq!(
                layout.settings.parent(),
                layout.record.parent(),
                "{platform:?}"
            );
            assert_eq!(
                layout.relay_ca.parent(),
                layout.record.parent(),
                "{platform:?}"
            );
            assert_eq!(layout.settings.file_name().unwrap(), "settings.toml");
            assert_eq!(layout.relay_ca.file_name().unwrap(), "relay-ca.pem");
            // Never the profile file of a program: on Windows the record lies
            // next to the installed program, where `cretlink.toml` would too.
            assert_ne!(
                Some(layout.settings.clone()),
                relay_profile::path_next_to(&layout.exe),
                "{platform:?}"
            );
        }
    }

    #[test]
    fn without_a_file_there_are_no_settings() {
        let home = Home::new("none", Platform::Linux);
        let settings = read(&home.layout).unwrap();
        assert!(settings.is_empty());
        // Reading creates nothing.
        assert!(!home.root.exists());
    }

    #[test]
    fn settings_are_written_and_read_back() {
        let home = Home::new("round-trip", Platform::Linux);
        let written = Settings {
            relay: Some(relay()),
            network_adapter: Some("vEthernet (Default Switch)".to_owned()),
        };
        write(&home.layout, &written).unwrap();
        let read_back = read(&home.layout).unwrap();
        let relay = read_back.relay.as_ref().unwrap();
        assert_eq!(relay.address().to_string(), "relay.example.org:443");
        assert_eq!(relay.ca(), Some(RELAY_CA_FILE));
        assert_eq!(relay.name(), Some("host-01.example.org"));
        assert_eq!(key_of(&read_back).as_deref(), Some(KEY));
        assert_eq!(
            read_back.network_adapter.as_deref(),
            Some("vEthernet (Default Switch)")
        );
        // No temporary file stays behind.
        assert!(!temporary_name(&home.layout.settings).exists());
    }

    #[test]
    fn each_part_is_kept_without_the_other() {
        let home = Home::new("parts", Platform::Linux);
        let only_network = Settings {
            relay: None,
            network_adapter: Some("Wi-Fi \"5 GHz\" \\ café 網路".to_owned()),
        };
        write(&home.layout, &only_network).unwrap();
        let read_back = read(&home.layout).unwrap();
        assert!(read_back.relay.is_none());
        assert_eq!(read_back.network_adapter, only_network.network_adapter);

        let only_relay = Settings {
            relay: Some(RelayProfile::from_fields("192.0.2.1:9970", None, None, None).unwrap()),
            network_adapter: None,
        };
        write(&home.layout, &only_relay).unwrap();
        let read_back = read(&home.layout).unwrap();
        assert_eq!(
            read_back.relay.unwrap().address().to_string(),
            "192.0.2.1:9970"
        );
        assert_eq!(read_back.network_adapter, None);
    }

    #[test]
    fn settings_that_hold_nothing_remove_the_file() {
        let home = Home::new("empty", Platform::Linux);
        // Nothing to write and nothing there: nothing is created.
        write(&home.layout, &Settings::default()).unwrap();
        assert!(!home.root.exists());
        write(
            &home.layout,
            &Settings {
                relay: Some(relay()),
                network_adapter: None,
            },
        )
        .unwrap();
        assert!(home.layout.settings.is_file());
        write(&home.layout, &Settings::default()).unwrap();
        assert!(!home.layout.settings.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_for_this_user_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = Home::new("mode", Platform::Linux);
        let settings = Settings {
            relay: Some(relay()),
            network_adapter: None,
        };
        write(&home.layout, &settings).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home.layout.settings), 0o600);
        // A file that was readable by others is replaced by one that is not.
        fs::set_permissions(&home.layout.settings, fs::Permissions::from_mode(0o644)).unwrap();
        write(&home.layout, &settings).unwrap();
        assert_eq!(mode(&home.layout.settings), 0o600);

        write_with_relay_ca(&home.layout, &settings, RelayCa::Replace(PEM)).unwrap();
        assert_eq!(mode(&home.layout.relay_ca), 0o600);
        assert_eq!(mode(&home.layout.settings), 0o600);
    }

    #[test]
    fn an_unknown_section_or_setting_is_an_error() {
        for (text, line, says) in [
            ("[proxy]\n", 1, "the sections [relay] and [network]"),
            (
                "[network]\nname = \"Wi-Fi\"\n",
                2,
                "not a setting of the network",
            ),
            (
                "[network]\nadapter = \"\"\n",
                2,
                "name of a network adapter",
            ),
            (
                "[network]\nadapter = \"a\\nb\"\n",
                2,
                "name of a network adapter",
            ),
            (
                "[relay]\nadress = \"relay.example.org:443\"\n",
                2,
                "not a setting of a relay",
            ),
            ("[relay]\nkey = \"abc\"\n", 1, "no `address`"),
            (
                "[network]\nadapter = Wi-Fi\n",
                2,
                "`adapter` must be text in double quotes",
            ),
        ] {
            let problem = parse(text).unwrap_err();
            assert_eq!(problem.line, Some(line), "{text:?}");
            assert!(problem.text.contains(says), "{text:?}: {}", problem.text);
        }
        let long = format!(
            "[network]\nadapter = \"{}\"\n",
            "a".repeat(MAX_ADAPTER_CHARS + 1)
        );
        assert!(parse(&long).is_err());
        // The certificate file of the settings is the one this module keeps.
        let other = parse("[relay]\naddress = \"relay.example.org:443\"\nca = \"other.pem\"\n")
            .unwrap_err();
        assert_eq!(other.field, Some(Field::Ca));
        assert!(!other.text.contains("other.pem"), "{}", other.text);
    }

    #[test]
    fn a_settings_file_that_cannot_be_used_names_itself_and_keeps_the_key_out() {
        let home = Home::new("malformed", Platform::Linux);
        fs::create_dir_all(home.layout.settings.parent().unwrap()).unwrap();
        fs::write(
            &home.layout.settings,
            format!("[relay]\naddress = \"relay.example.org:443\"\nkey = {KEY}\n"),
        )
        .unwrap();
        let error = read(&home.layout).unwrap_err();
        assert_eq!(error.file, home.layout.settings);
        assert_eq!(error.problem.line, Some(3));
        assert!(!error.to_string().contains(KEY), "{error}");
    }

    #[test]
    fn an_adapter_name_that_cannot_be_kept_is_refused_and_changes_nothing() {
        let home = Home::new("adapter", Platform::Linux);
        write(
            &home.layout,
            &Settings {
                relay: None,
                network_adapter: Some("Wi-Fi".to_owned()),
            },
        )
        .unwrap();
        for bad in ["", "  ", "two\nlines", &"a".repeat(MAX_ADAPTER_CHARS + 1)] {
            let error = write(
                &home.layout,
                &Settings {
                    relay: None,
                    network_adapter: Some(bad.to_owned()),
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(
            read(&home.layout).unwrap().network_adapter.as_deref(),
            Some("Wi-Fi")
        );
    }

    #[test]
    fn the_certificate_file_is_kept_next_to_the_settings_and_read_through_the_profile() {
        let home = Home::new("ca", Platform::MacOs);
        let with_relay = Settings {
            relay: Some(relay()),
            network_adapter: None,
        };
        write_with_relay_ca(&home.layout, &with_relay, RelayCa::Replace(PEM)).unwrap();
        let settings = read(&home.layout).unwrap();
        let found = relay_profile::read_ca(&home.layout.settings, settings.relay.as_ref().unwrap())
            .unwrap();
        assert_eq!(found.as_deref(), Some(PEM));
        // No temporary file stays behind.
        assert!(!temporary_name(&home.layout.relay_ca).exists());

        // Kept: the file stays what it is while the settings change.
        let moved = Settings {
            relay: Some(
                RelayProfile::from_fields("192.0.2.1:9970", None, Some(RELAY_CA_FILE), None)
                    .unwrap(),
            ),
            network_adapter: None,
        };
        write_with_relay_ca(&home.layout, &moved, RelayCa::Keep).unwrap();
        assert_eq!(fs::read(&home.layout.relay_ca).unwrap(), PEM);
        assert_eq!(
            read(&home.layout)
                .unwrap()
                .relay
                .unwrap()
                .address()
                .to_string(),
            "192.0.2.1:9970"
        );

        // Removed, and nothing to remove is fine.
        let without = Settings::default();
        write_with_relay_ca(&home.layout, &without, RelayCa::Remove).unwrap();
        write_with_relay_ca(&home.layout, &without, RelayCa::Remove).unwrap();
        assert!(!home.layout.relay_ca.exists());
        assert!(!home.layout.settings.exists());
    }

    #[test]
    fn only_the_certificates_of_a_chosen_file_are_kept() {
        let home = Home::new("ca-only", Platform::Linux);
        let with_relay = Settings {
            relay: Some(relay()),
            network_adapter: None,
        };
        // The file of an authority as some tools write it: its private key
        // first, then its certificate.
        let end = KEY_MARKER.replace("BEGIN", "END");
        let whole = format!(
            "# kept by Sam\n{KEY_MARKER}bm90IGEga2V5\n{end}{}",
            String::from_utf8_lossy(PEM)
        );
        write_with_relay_ca(
            &home.layout,
            &with_relay,
            RelayCa::Replace(whole.as_bytes()),
        )
        .unwrap();
        assert_eq!(fs::read(&home.layout.relay_ca).unwrap(), PEM);

        // A text without a certificate, or a larger one than a certificate
        // file may be, is refused before anything changes.
        let before = fs::read(&home.layout.settings).unwrap();
        let other = Settings {
            relay: Some(
                RelayProfile::from_fields("192.0.2.1:9970", None, Some(RELAY_CA_FILE), None)
                    .unwrap(),
            ),
            network_adapter: None,
        };
        let mut too_large = Vec::new();
        while too_large.len() as u64 <= MAX_CA_BYTES {
            too_large.extend_from_slice(PEM);
        }
        for refused in [KEY_MARKER.as_bytes(), b"", too_large.as_slice()] {
            let error =
                write_with_relay_ca(&home.layout, &other, RelayCa::Replace(refused)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(fs::read(&home.layout.settings).unwrap(), before);
            assert_eq!(fs::read(&home.layout.relay_ca).unwrap(), PEM);
            assert!(!temporary_name(&home.layout.relay_ca).exists());
        }
    }

    /// A folder with something in it where a file should go: nothing can be
    /// written or moved there, and a file cannot be removed there either.
    fn block(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("in-the-way"), b"").unwrap();
    }

    #[test]
    fn a_change_that_fails_leaves_the_settings_and_the_certificate_file_as_they_were() {
        let other_pem = b"-----BEGIN CERTIFICATE-----\nMIIC\n-----END CERTIFICATE-----\n";
        let before = Settings {
            relay: Some(relay()),
            network_adapter: Some("Wi-Fi".to_owned()),
        };
        let after = Settings {
            relay: Some(
                RelayProfile::from_fields(
                    "host-01.example.org:9970",
                    Some("another-key"),
                    Some(RELAY_CA_FILE),
                    None,
                )
                .unwrap(),
            ),
            network_adapter: None,
        };

        // The settings cannot be written: the certificate file of before is
        // still the one in use, and nothing of the new one stays behind.
        let home = Home::new("fails-settings", Platform::Linux);
        write_with_relay_ca(&home.layout, &before, RelayCa::Replace(PEM)).unwrap();
        let settings_before = fs::read(&home.layout.settings).unwrap();
        block(&temporary_name(&home.layout.settings));
        for ca in [RelayCa::Replace(other_pem), RelayCa::Remove, RelayCa::Keep] {
            write_with_relay_ca(&home.layout, &after, ca).unwrap_err();
            assert_eq!(fs::read(&home.layout.settings).unwrap(), settings_before);
            assert_eq!(fs::read(&home.layout.relay_ca).unwrap(), PEM);
            assert!(!temporary_name(&home.layout.relay_ca).exists());
        }

        // The certificate file cannot take its place: the settings of before
        // are put back.
        let home = Home::new("fails-ca", Platform::Linux);
        write(&home.layout, &before).unwrap();
        let settings_before = fs::read(&home.layout.settings).unwrap();
        block(&home.layout.relay_ca);
        for ca in [RelayCa::Replace(other_pem), RelayCa::Remove] {
            write_with_relay_ca(&home.layout, &after, ca).unwrap_err();
            assert_eq!(fs::read(&home.layout.settings).unwrap(), settings_before);
            assert!(home.layout.relay_ca.join("in-the-way").exists());
            assert!(!temporary_name(&home.layout.relay_ca).exists());
            assert!(!temporary_name(&home.layout.settings).exists());
        }
        let kept = read(&home.layout).unwrap();
        assert_eq!(key_of(&kept).as_deref(), Some(KEY));
        assert_eq!(kept.network_adapter.as_deref(), Some("Wi-Fi"));

        // There were no settings before: there are none afterwards.
        let home = Home::new("fails-first", Platform::Linux);
        block(&home.layout.relay_ca);
        write_with_relay_ca(&home.layout, &after, RelayCa::Replace(other_pem)).unwrap_err();
        assert!(!home.layout.settings.exists());
        assert!(!temporary_name(&home.layout.relay_ca).exists());
    }

    #[test]
    fn the_written_text_is_the_small_form_a_person_can_read() {
        let text = Settings {
            relay: Some(
                RelayProfile::from_fields("relay.example.org:443", Some("abc"), None, None)
                    .unwrap(),
            ),
            network_adapter: Some("Wi-Fi".to_owned()),
        }
        .to_text();
        assert!(
            text.starts_with("# Settings of the installed CretLink."),
            "{}",
            *text
        );
        assert!(
            text.ends_with(
                "\n[relay]\naddress = \"relay.example.org:443\"\nkey = \"abc\"\n\n[network]\nadapter = \"Wi-Fi\"\n"
            ),
            "{}",
            *text
        );
    }
}
