//! The relay profile: which relay a copy of CretLink uses (ST-M007-011,
//! ADR-0016 §2 and §4, REQ-F-022).
//!
//! When two computers are not on one network, their copies of CretLink meet
//! on a relay that the technician or their organisation hosts. A **relay
//! profile** tells a copy which relay that is:
//!
//! | Setting   | Meaning |
//! |-----------|---------|
//! | `address` | The relay's host name and port, `relay.example.org:443`. |
//! | `key`     | The admission key of this copy, when the relay asks for one. |
//! | `ca`      | A PEM file next to the profile, for a relay whose certificate comes from a private authority. |
//! | `name`    | The name in the relay's certificate, when it differs from the host of `address`. |
//!
//! A portable copy and every customer copy read it from a file
//! [`PROFILE_FILE`] next to the program ([`read_next_to`]):
//!
//! ```toml
//! [relay]
//! address = "relay.example.org:443"
//! key = "5f2c9d0e…"
//! ```
//!
//! The installed technician copy can also keep one in its settings (the
//! `settings` module of this crate). Which of the two a copy uses is decided
//! by the application; a file next to the program wins.
//!
//! # A profile is addressing, never authority
//!
//! Nothing in a profile grants control (ADR-0016 §2, §4). The address says
//! where two copies meet, and the key only admits a copy to the forwarder:
//! the relay uses it to keep strangers out and to count per customer. Pairing,
//! the approval code that two people compare and the customer's consent
//! decide about a session, exactly as on one network.
//!
//! # Always TLS
//!
//! A profile has no setting that turns TLS off. The application speaks `wss`
//! to the relay, checks its certificate against the system's authorities, or
//! against `ca` when the profile names one, and checks that the certificate
//! carries the relay's name ([`RelayProfile::server_name`]). An address with
//! `ws://` or any other scheme is refused.
//!
//! # A profile file is untrusted input
//!
//! Somebody else may have written the file. It is read up to
//! [`MAX_PROFILE_BYTES`], every setting is bounded, and an unknown setting or
//! section is an error. A [`Problem`] names the line and the setting in plain
//! words and **never holds text from the file**, so a key that was typed in
//! the wrong place is not repeated on a screen or in a log. The key itself is
//! an [`AdmissionToken`]: erased when dropped, and redacted in `Debug`.

pub(crate) mod document;

use std::fmt;
use std::fs;
use std::io::{self, Read as _};
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};

use cretlink_relay::{AdmissionToken, MAX_PREAMBLE_TOKEN_BYTES, canonical_relay_host};
use zeroize::Zeroizing;

/// The name of the profile file next to a program.
pub const PROFILE_FILE: &str = "cretlink.toml";

/// The section of [`PROFILE_FILE`] that holds the relay profile.
pub const RELAY_SECTION: &str = "relay";

/// The largest profile file that is read. A profile is a few short lines.
pub const MAX_PROFILE_BYTES: u64 = 16 * 1024;

/// The largest certificate authority file that is read.
pub const MAX_CA_BYTES: u64 = 256 * 1024;

/// The longest admission key: what a rendezvous preamble can carry.
pub const MAX_KEY_BYTES: usize = MAX_PREAMBLE_TOKEN_BYTES;

/// The longest name of a certificate authority file.
pub const MAX_CA_NAME_CHARS: usize = 100;

/// The longest `address`: a DNS name, a colon and a port.
const MAX_ADDRESS_BYTES: usize = 253 + 1 + 5 + 2;

/// A setting of a relay profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The relay's host name and port.
    Address,
    /// The admission key.
    Key,
    /// The certificate authority file.
    Ca,
    /// The name in the relay's certificate.
    Name,
}

impl Field {
    /// Every setting, in the order a profile is written in.
    pub const ALL: [Self; 4] = [Self::Address, Self::Key, Self::Ca, Self::Name];

    /// The name of the setting as the file writes it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Address => "address",
            Self::Key => "key",
            Self::Ca => "ca",
            Self::Name => "name",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.name() == name)
    }
}

/// The settings a message about a profile may name.
pub(crate) const KNOWN_FIELDS: [document::Known; 4] = [
    known(Field::Address),
    known(Field::Key),
    known(Field::Ca),
    known(Field::Name),
];

const fn known(field: Field) -> document::Known {
    document::Known {
        name: field.name(),
        field: Some(field),
    }
}

/// What is wrong with a profile, in plain words (REQ-N-015).
///
/// `text` is a sentence without its full stop. It names the setting and never
/// holds a value from the file or the form: not the key, and nothing that was
/// typed where the key should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The line of the file, counted from 1, when the problem is on one line.
    pub line: Option<usize>,
    /// The setting concerned, when it is one of a relay profile.
    pub field: Option<Field>,
    /// What is wrong.
    pub text: String,
}

impl Problem {
    pub(crate) fn at(line: usize, text: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            field: None,
            text: text.into(),
        }
    }

    pub(crate) fn file(text: impl Into<String>) -> Self {
        Self {
            line: None,
            field: None,
            text: text.into(),
        }
    }

    fn of(field: Field, text: impl Into<String>) -> Self {
        Self {
            line: None,
            field: Some(field),
            text: text.into(),
        }
    }

    fn on_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.text),
            None => f.write_str(&self.text),
        }
    }
}

/// A profile file that cannot be used: the file and what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError {
    /// The file that was read.
    pub file: PathBuf,
    /// What is wrong with it.
    pub problem: Problem,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the relay settings in {} cannot be used: {}",
            self.file.display(),
            self.problem
        )
    }
}

impl std::error::Error for ProfileError {}

/// Where a relay listens: a host and a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAddress {
    host: String,
    port: u16,
}

impl RelayAddress {
    /// Reads `host:port`. The host is a DNS name or an IP address, an IPv6
    /// address in square brackets; it is kept in the form two copies agree on
    /// (`cretlink_relay::canonical_relay_host`).
    ///
    /// # Errors
    ///
    /// Returns what is wrong in plain words. The message never repeats the
    /// input.
    pub fn parse(text: &str) -> Result<Self, Problem> {
        let problem = |text: &str| Problem::of(Field::Address, text);
        let text = text.trim();
        if text.is_empty() {
            return Err(problem(
                "`address` is empty; write the relay's host name and port, like relay.example.org:443",
            ));
        }
        if text.len() > MAX_ADDRESS_BYTES {
            return Err(problem("`address` is too long for a host name and a port"));
        }
        if text.contains("://") || text.contains('/') {
            return Err(problem(
                "`address` is the relay's host name and port only, like relay.example.org:443, \
                 without wss:// and without a path; CretLink always uses TLS",
            ));
        }
        let (host, port) = match text.strip_prefix('[') {
            Some(rest) => rest.split_once("]:").ok_or_else(|| {
                problem("`address` needs a port after the address in brackets, like [2001:db8::1]:443")
            })?,
            None => text.rsplit_once(':').ok_or_else(|| {
                problem("`address` needs a port; write the relay's host name and port, like relay.example.org:443")
            })?,
        };
        if !text.starts_with('[') && host.contains(':') {
            return Err(problem(
                "an IPv6 address in `address` stands in square brackets, like [2001:db8::1]:443",
            ));
        }
        let port = port
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| port.parse::<u16>().ok())
            .flatten()
            .filter(|port| *port != 0)
            .ok_or_else(|| problem("the port in `address` is a number from 1 to 65535"))?;
        if text.starts_with('[') && host.parse::<Ipv6Addr>().is_err() {
            return Err(problem(
                "square brackets in `address` are for an IPv6 address, like [2001:db8::1]:443",
            ));
        }
        let host = canonical_relay_host(host).map_err(|_| {
            problem(
                "the host in `address` is not a host name or an IP address in its usual spelling",
            )
        })?;
        Ok(Self { host, port })
    }

    /// The host, in its canonical form: a DNS name in lower case, or an IP
    /// address in its standard text form.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

/// `host:port`, an IPv6 address in square brackets.
impl fmt::Display for RelayAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host.parse::<IpAddr>() {
            Ok(IpAddr::V6(_)) => write!(f, "[{}]:{}", self.host, self.port),
            _ => write!(f, "{}:{}", self.host, self.port),
        }
    }
}

/// A relay profile: where the relay is, and what this copy presents to it.
///
/// Addressing only. Holding a profile opens no connection and grants nothing
/// (ADR-0016 §2, §4).
#[derive(Clone)]
pub struct RelayProfile {
    address: RelayAddress,
    key: Option<AdmissionToken>,
    ca: Option<String>,
    name: Option<String>,
}

/// The key is redacted: `Debug` says only whether there is one.
impl fmt::Debug for RelayProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayProfile")
            .field("address", &self.address)
            .field("has_key", &self.key.is_some())
            .field("ca", &self.ca)
            .field("name", &self.name)
            .finish()
    }
}

impl RelayProfile {
    /// Builds a profile from the texts of its settings, as a file or a form
    /// gives them. An absent setting is `None`; a setting that is present and
    /// empty is refused.
    ///
    /// # Errors
    ///
    /// Returns the first setting that cannot be used, in plain words. The
    /// message never repeats a value.
    pub fn from_fields(
        address: &str,
        key: Option<&str>,
        ca: Option<&str>,
        name: Option<&str>,
    ) -> Result<Self, Problem> {
        let address = RelayAddress::parse(address)?;
        let key = key.map(parse_key).transpose()?;
        let ca = ca.map(parse_ca).transpose()?;
        let name = name.map(parse_name).transpose()?;
        Ok(Self {
            address,
            key,
            ca,
            name,
        })
    }

    /// Where the relay listens.
    #[must_use]
    pub fn address(&self) -> &RelayAddress {
        &self.address
    }

    /// The admission key of this copy, when the profile has one. A key admits
    /// to the forwarder and decides nothing about a session.
    #[must_use]
    pub fn key(&self) -> Option<&AdmissionToken> {
        self.key.as_ref()
    }

    /// The name of the certificate authority file next to the profile.
    #[must_use]
    pub fn ca(&self) -> Option<&str> {
        self.ca.as_deref()
    }

    /// The name in the relay's certificate, when the profile gives one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The name the relay's certificate must carry: `name` when the profile
    /// gives one, and the host of `address` otherwise.
    ///
    /// It is also the relay's host name for everything else: what both people
    /// are shown before they connect, and what the rendezvous ticket of a
    /// relay code is derived with (ST-M007-012). Two copies meet when they
    /// name the same relay here, also when one of them reaches it by another
    /// address.
    #[must_use]
    pub fn server_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.address.host)
    }

    /// The same profile with another key, or none.
    #[must_use]
    pub fn with_key(mut self, key: Option<AdmissionToken>) -> Self {
        self.key = key;
        self
    }

    /// The same profile with another certificate authority file, or none.
    ///
    /// # Errors
    ///
    /// Returns the problem when `ca` is not the plain name of a file.
    pub fn with_ca(mut self, ca: Option<&str>) -> Result<Self, Problem> {
        self.ca = ca.map(parse_ca).transpose()?;
        Ok(self)
    }

    /// Writes the profile as the `[relay]` section of a file, which
    /// [`parse`] reads back.
    ///
    /// The text holds the key in readable form. It is erased when dropped;
    /// write it to a file and nowhere else.
    #[must_use]
    pub fn to_section(&self) -> Zeroizing<String> {
        let mut out = Zeroizing::new(format!("[{RELAY_SECTION}]\n"));
        let mut line = |field: Field, value: &str| {
            out.push_str(field.name());
            out.push_str(" = ");
            out.push_str(&document::quoted(value));
            out.push('\n');
        };
        line(Field::Address, &self.address.to_string());
        if let Some(key) = &self.key {
            // A key is printable ASCII ([`parse_key`]), so it is text.
            line(Field::Key, &String::from_utf8_lossy(key.as_bytes()));
        }
        if let Some(ca) = &self.ca {
            line(Field::Ca, ca);
        }
        if let Some(name) = &self.name {
            line(Field::Name, name);
        }
        out
    }
}

fn parse_key(text: &str) -> Result<AdmissionToken, Problem> {
    let problem = |text: &str| Problem::of(Field::Key, text);
    if text.is_empty() {
        return Err(problem(
            "`key` is empty; leave it out when the relay asks for no key",
        ));
    }
    if text.len() > MAX_KEY_BYTES {
        return Err(problem(&format!(
            "`key` is longer than {MAX_KEY_BYTES} characters"
        )));
    }
    if !text.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(problem(
            "`key` holds a blank or a character that is not plain text",
        ));
    }
    Ok(AdmissionToken::from_text(text))
}

fn parse_ca(text: &str) -> Result<String, Problem> {
    let problem = |text: &str| Problem::of(Field::Ca, text);
    if text.is_empty() {
        return Err(problem(
            "`ca` is empty; leave it out when the relay's certificate comes from a public authority",
        ));
    }
    let plain = text.chars().count() <= MAX_CA_NAME_CHARS
        && text != "."
        && text != ".."
        && !text
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        && text.trim() == text;
    if !plain {
        return Err(problem(
            "`ca` is the name of a file next to this profile, without a folder",
        ));
    }
    Ok(text.to_owned())
}

fn parse_name(text: &str) -> Result<String, Problem> {
    let problem = |text: &str| Problem::of(Field::Name, text);
    if text.trim().is_empty() {
        return Err(problem(
            "`name` is empty; leave it out when the certificate carries the host of `address`",
        ));
    }
    canonical_relay_host(text).map_err(|_| {
        problem(
            "`name` is the name in the relay's certificate: a host name, or an IP address in its \
             usual spelling",
        )
    })
}

/// Reads the relay profile of a section of a file read by
/// [`document::parse`].
pub(crate) fn from_section(section: &document::Section) -> Result<RelayProfile, Problem> {
    let mut values: [Option<&document::Setting>; 4] = [None; 4];
    for setting in &section.settings {
        let Some(field) = Field::parse(&setting.name) else {
            return Err(Problem::at(
                setting.line,
                "this is not a setting of a relay; a relay has `address`, `key`, `ca` and `name`",
            ));
        };
        values[field as usize] = Some(setting);
    }
    let text = |field: Field| values[field as usize].map(|setting| setting.value.as_str());
    let Some(address) = text(Field::Address) else {
        return Err(Problem {
            line: Some(section.line),
            field: Some(Field::Address),
            text: "the relay has no `address`; write the relay's host name and port, like \
                   address = \"relay.example.org:443\""
                .to_owned(),
        });
    };
    RelayProfile::from_fields(
        address,
        text(Field::Key),
        text(Field::Ca),
        text(Field::Name),
    )
    .map_err(|problem| match problem.field {
        Some(field) => match values[field as usize] {
            Some(setting) => problem.on_line(setting.line),
            None => problem,
        },
        None => problem,
    })
}

/// Reads the text of a profile file: the relay profile of its `[relay]`
/// section, or `None` when the file has no such section.
///
/// # Errors
///
/// Returns what is wrong, with the line. An unknown section or setting is an
/// error.
pub fn parse(text: &str) -> Result<Option<RelayProfile>, Problem> {
    let document = document::parse(text, &KNOWN_FIELDS)?;
    if let Some(other) = document
        .sections
        .iter()
        .find(|section| section.name != RELAY_SECTION)
    {
        return Err(Problem::at(
            other.line,
            "this file has one section, [relay]",
        ));
    }
    document
        .section(RELAY_SECTION)
        .map(from_section)
        .transpose()
}

/// The profile file next to the program `exe`.
#[must_use]
pub fn path_next_to(exe: &Path) -> Option<PathBuf> {
    exe.parent().map(|folder| folder.join(PROFILE_FILE))
}

/// Reads a text file of at most `limit` bytes. `Ok(None)` when there is no
/// such file.
pub(crate) fn read_bounded(
    path: &Path,
    limit: u64,
    what: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, Problem> {
    let unreadable = |error: &io::Error| {
        Problem::file(format!(
            "{what} could not be read ({})",
            io_reason(error.kind())
        ))
    };
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(&error)),
    };
    if !meta.is_file() {
        return Err(Problem::file(format!("{what} is not a file")));
    }
    let too_large = || Problem::file(format!("{what} is larger than {} KiB", limit / 1024));
    if meta.len() > limit {
        return Err(too_large());
    }
    let file = fs::File::open(path).map_err(|error| unreadable(&error))?;
    let mut bytes = Zeroizing::new(Vec::new());
    // One byte more than the limit shows a file that grew since it was
    // measured.
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unreadable(&error))?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(Some(bytes))
}

/// Why a file could not be read, in words that hold nothing of the system's
/// own message (which may name paths).
fn io_reason(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::PermissionDenied => "this account may not read it",
        io::ErrorKind::NotFound => "it is not there",
        _ => "the system could not open it",
    }
}

/// Reads the profile file `path`: its relay profile, or `None` when the file
/// is not there or has no `[relay]` section.
///
/// # Errors
///
/// Returns the file and what is wrong with it. Nothing was connected to.
pub fn read_file(path: &Path) -> Result<Option<RelayProfile>, ProfileError> {
    let in_file = |problem: Problem| ProfileError {
        file: path.to_path_buf(),
        problem,
    };
    let Some(bytes) = read_bounded(path, MAX_PROFILE_BYTES, "the file").map_err(in_file)? else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| in_file(Problem::file("the file is not text (UTF-8)")))?;
    parse(text).map_err(in_file)
}

/// Reads the profile file next to the program `exe`
/// ([`path_next_to`], [`read_file`]). Returns the file with its profile.
///
/// # Errors
///
/// Returns the file and what is wrong with it.
pub fn read_next_to(exe: &Path) -> Result<Option<(PathBuf, RelayProfile)>, ProfileError> {
    let Some(path) = path_next_to(exe) else {
        return Ok(None);
    };
    Ok(read_file(&path)?.map(|profile| (path, profile)))
}

/// Reads a certificate authority file: at most [`MAX_CA_BYTES`], and with at
/// least one PEM certificate in it. Whether that certificate can be used is
/// decided when the TLS settings are built from it.
///
/// # Errors
///
/// Returns what is wrong with the file, in words that name it as `what`.
/// `Ok(None)` when the file is not there.
fn read_certificates(path: &Path, what: &str) -> Result<Option<Vec<u8>>, Problem> {
    let Some(bytes) = read_bounded(path, MAX_CA_BYTES, what)? else {
        return Ok(None);
    };
    if !contains(&bytes, b"-----BEGIN CERTIFICATE-----") {
        return Err(Problem::of(
            Field::Ca,
            format!(
                "{what} holds no certificate; a PEM file has a line -----BEGIN CERTIFICATE-----"
            ),
        ));
    }
    Ok(Some(bytes.to_vec()))
}

/// Reads a certificate authority file that a person chose, to keep a copy of
/// it with a profile.
///
/// # Errors
///
/// Returns what is wrong with the file: it is not there, is no file, is
/// larger than [`MAX_CA_BYTES`] or holds no PEM certificate.
pub fn read_certificate_file(path: &Path) -> Result<Vec<u8>, Problem> {
    let what = "the certificate file";
    read_certificates(path, what)
        .map_err(|problem| Problem::of(Field::Ca, problem.text))?
        .ok_or_else(|| Problem::of(Field::Ca, format!("{what} is not there")))
}

/// Reads the certificate authority file that `profile` names, from the folder
/// of the profile file `profile_file`. `Ok(None)` when the profile names none.
///
/// The file must hold at least one PEM certificate; whether that certificate
/// can be used is decided when the TLS settings are built from it.
///
/// # Errors
///
/// Returns the profile file and what is wrong with its `ca`.
pub fn read_ca(
    profile_file: &Path,
    profile: &RelayProfile,
) -> Result<Option<Vec<u8>>, ProfileError> {
    let Some(name) = profile.ca() else {
        return Ok(None);
    };
    let in_file = |text: &str| ProfileError {
        file: profile_file.to_path_buf(),
        problem: Problem::of(Field::Ca, text),
    };
    let folder = profile_file
        .parent()
        .ok_or_else(|| in_file("`ca` names a file next to this file, which has no folder"))?;
    read_certificates(&folder.join(name), "the file that `ca` names")
        .map_err(|problem| in_file(&problem.text))?
        .ok_or_else(|| in_file("`ca` names a file that is not in the folder of this file"))
        .map(Some)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests;
