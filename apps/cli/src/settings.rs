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
//! When the relay's certificate comes from a private authority, a copy of that
//! authority's PEM file stands next to it as [`Layout::relay_ca`], and the
//! profile names it.
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

/// The names a message about the settings may use.
const KNOWN_NAMES: [&str; 5] = ["address", "key", "ca", "name", ADAPTER];

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
    let document = document::parse(text, &KNOWN_NAMES)?;
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
/// the file in one step. Settings that hold nothing remove the file.
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

/// Keeps a copy of the certificate authority file of the relay in the
/// settings ([`Layout::relay_ca`]). The profile names it as
/// [`RELAY_CA_FILE`].
///
/// # Errors
///
/// Returns `InvalidInput` when `pem` is larger than a certificate authority
/// file may be, or the error of the file system.
pub fn write_relay_ca(layout: &Layout, pem: &[u8]) -> io::Result<()> {
    if pem.len() as u64 > MAX_CA_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the certificate file is too large",
        ));
    }
    write_private(&layout.relay_ca, pem)
}

/// Removes the kept certificate authority file. Nothing to remove is fine.
///
/// # Errors
///
/// Returns the error of the file system.
pub fn remove_relay_ca(layout: &Layout) -> io::Result<()> {
    remove_file_if_exists(&layout.relay_ca).map(drop)
}

/// Writes `bytes` to `path` through a temporary file in the same folder that
/// only this user can read, so that the file is never there half written and
/// never readable by others on the way.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_name(path);
    // One left behind by a run that was interrupted.
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
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{BaseDirs, Platform};
    use std::path::PathBuf;

    const KEY: &str = "5f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5";

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

        write_relay_ca(&home.layout, b"-----BEGIN CERTIFICATE-----\n").unwrap();
        assert_eq!(mode(&home.layout.relay_ca), 0o600);
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
        let pem = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        write_relay_ca(&home.layout, pem).unwrap();
        write(
            &home.layout,
            &Settings {
                relay: Some(relay()),
                network_adapter: None,
            },
        )
        .unwrap();
        let settings = read(&home.layout).unwrap();
        let found = relay_profile::read_ca(&home.layout.settings, settings.relay.as_ref().unwrap())
            .unwrap();
        assert_eq!(found.as_deref(), Some(&pem[..]));

        remove_relay_ca(&home.layout).unwrap();
        remove_relay_ca(&home.layout).unwrap();
        assert!(!home.layout.relay_ca.exists());
        let too_large = vec![b'A'; usize::try_from(MAX_CA_BYTES).unwrap() + 1];
        assert_eq!(
            write_relay_ca(&home.layout, &too_large).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!home.layout.relay_ca.exists());
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
