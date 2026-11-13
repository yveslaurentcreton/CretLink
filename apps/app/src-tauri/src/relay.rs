//! The relay of this copy (ST-M007-011, ADR-0016, REQ-F-022).
//!
//! When two computers are not on one network, their copies of CretLink meet
//! on a relay that the technician hosts. This module answers, for either
//! role, which relay this copy uses, and reaches it:
//!
//! - **Where the profile comes from** ([`next_to_program`], [`in_settings`],
//!   [`TechnicianRelays`]). A file `cretlink.toml` next to the program is read
//!   by both roles: that is how a portable technician copy and every customer
//!   copy learn their relay. The installed technician copy can also keep a
//!   profile in its settings; a file next to the program wins. The customer
//!   role reads the file next to its program and nothing else, and no part of
//!   this module that it uses writes anything.
//! - **No profile, no relay.** Without a profile nothing here is offered and
//!   nothing is dialled; the session on one network is unchanged (REQ-N-003).
//! - **Always TLS** ([`Relay::connector`], [`EndpointDial`]). A relay is
//!   spoken to over `wss` only. Its certificate is checked against the
//!   system's authorities, or against the certificate authority file the
//!   profile names, and must carry the relay's name ([`Relay::host`]). When
//!   that fails nothing is sent and nothing falls back to a connection
//!   without TLS.
//! - **Addressing only.** A profile grants nothing (ADR-0016 §2, §4): the
//!   relay only lets two connections find each other, and pairing, the
//!   approval code and the customer's consent decide about a session exactly
//!   as on one network. The admission key is held as an [`AdmissionToken`]
//!   and never leaves this side except in the rendezvous preamble: no view,
//!   no message and no `Debug` output of this module holds it.
//! - **Failures in plain words** ([`failure_words`]). Each way a relay can
//!   fail to be reached has words of its own and a next step (REQ-N-015).

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use cretlink_cli::install::Layout;
use cretlink_cli::relay_profile::{self, Field, Problem, ProfileError, RelayProfile};
use cretlink_cli::settings;
use cretlink_relay::RelayRefusal;
use cretlink_session::{
    AdmissionToken, Dial, RELAY_CONNECT_TIMEOUT, RelayFailure, RendezvousTicket, SessionError,
    WssConnector, WssRelayDial,
};
use serde::Serialize;

/// The most addresses of a relay's name that are tried, in the order the
/// system gives them.
const MAX_ADDRESSES: usize = 4;

/// Where a relay profile comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// The file `cretlink.toml` next to the program.
    File(PathBuf),
    /// The settings of the installed copy.
    Settings(PathBuf),
}

impl Source {
    /// The file the profile was read from.
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::File(path) | Self::Settings(path) => path,
        }
    }
}

/// A relay this copy can use: a profile that was read and checked, with the
/// certificate authority it names.
#[derive(Clone)]
pub(crate) struct Relay {
    source: Source,
    profile: RelayProfile,
    /// The TLS settings that trust the profile's certificate authority file
    /// and nothing else. `None` when the profile names no file: the system's
    /// authorities are then read when the relay is first reached.
    pinned: Option<WssConnector>,
}

/// The key is not in it: the profile's own `Debug` says only whether there is
/// one.
impl fmt::Debug for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Relay")
            .field("source", &self.source)
            .field("profile", &self.profile)
            .field("pinned", &self.pinned.is_some())
            .finish()
    }
}

impl Relay {
    /// Checks a profile that was read from `source`, with the certificate
    /// authority file it names next to that file.
    fn checked(source: Source, profile: RelayProfile) -> Result<Self, ProfileError> {
        let pem = relay_profile::read_ca(source.path(), &profile)?;
        Self::with_ca(source, profile, pem.as_deref())
    }

    /// A relay from a profile and the PEM of the certificate authority it
    /// names, when it names one.
    ///
    /// The TLS settings for that authority are built here, before any
    /// connection, so that a file without a certificate that can be used is
    /// said as a problem of the profile and not found out while dialling.
    pub(crate) fn with_ca(
        source: Source,
        profile: RelayProfile,
        ca_pem: Option<&[u8]>,
    ) -> Result<Self, ProfileError> {
        let unusable = |text: &str| ProfileError {
            file: source.path().to_path_buf(),
            problem: Problem {
                line: None,
                field: Some(Field::Ca),
                text: text.to_owned(),
            },
        };
        let pinned = match (profile.ca(), ca_pem) {
            (None, _) => None,
            (Some(_), None) => {
                return Err(unusable(
                    "the certificate file that `ca` names is not there",
                ));
            }
            (Some(_), Some(pem)) => Some(WssRelayDial::connector(Some(pem)).map_err(|_| {
                unusable("the file that `ca` names holds no certificate that can be used")
            })?),
        };
        Ok(Self {
            source,
            profile,
            pinned,
        })
    }

    /// The profile itself. It holds the admission key: write it to the
    /// settings and nowhere else.
    pub(crate) fn profile(&self) -> &RelayProfile {
        &self.profile
    }

    /// The relay's host name: the name its certificate must carry. It is what
    /// both people are shown before they connect through it.
    pub(crate) fn host(&self) -> &str {
        self.profile.server_name()
    }

    /// The relay as a screen shows it.
    pub(crate) fn view(&self) -> RelayView {
        RelayView::of(&self.profile)
    }

    /// The TLS settings to reach this relay with: the profile's certificate
    /// authority file when it names one, and the system's authorities
    /// otherwise. Reading those can take a moment, so it happens off the
    /// calling task.
    ///
    /// # Errors
    ///
    /// [`RelayFailure::Settings`] when the system offers no authorities.
    /// Nothing was dialled.
    pub(crate) async fn connector(&self) -> Result<WssConnector, RelayFailure> {
        if let Some(pinned) = &self.pinned {
            return Ok(pinned.clone());
        }
        let built = tokio::task::spawn_blocking(|| WssRelayDial::connector(None)).await;
        match built {
            Ok(Ok(connector)) => Ok(connector),
            Ok(Err(SessionError::Relay(failure))) => Err(failure),
            Ok(Err(other)) => Err(RelayFailure::Settings(other.to_string())),
            Err(_) => Err(RelayFailure::Settings(
                "the system's certificate authorities could not be read".to_owned(),
            )),
        }
    }

    /// A dial to this relay that presents `ticket` and the profile's key.
    pub(crate) fn dial(&self, connector: WssConnector, ticket: RendezvousTicket) -> EndpointDial {
        let address = self.profile.address();
        EndpointDial {
            host: address.host().to_owned(),
            port: address.port(),
            server_name: self.profile.server_name().to_owned(),
            connector,
            ticket,
            token: self.profile.key().cloned(),
        }
    }

    /// Reaches the relay once and says in words what came of it
    /// ([`TestResult`]): the connection, the certificate, and whether the
    /// relay admits this copy with its key.
    ///
    /// It presents a rendezvous ticket that was drawn for this test alone, so
    /// it meets nobody, and it hangs up as soon as the relay has answered.
    /// Nothing of a session is part of it.
    pub(crate) async fn test(&self, audience: Audience) -> TestResult {
        let view = self.view();
        let failed =
            |failure: &RelayFailure| TestResult::failed(failure_words(failure, &view, audience));
        let connector = match self.connector().await {
            Ok(connector) => connector,
            Err(failure) => return failed(&failure),
        };
        let Ok(ticket) = RendezvousTicket::generate() else {
            return failed(&RelayFailure::Settings(
                "this computer gave no randomness for the test".to_owned(),
            ));
        };
        match self.dial(connector, ticket).dial().await {
            // The leg is dropped at once: the relay then forgets it.
            Ok(_leg) => TestResult::admitted(&view, self.pinned.is_some()),
            Err(SessionError::Relay(failure)) => failed(&failure),
            Err(other) => failed(&RelayFailure::Settings(other.to_string())),
        }
    }
}

/// What this copy knows about a relay from one place.
#[derive(Debug, Clone)]
pub(crate) enum Lookup {
    /// No profile there: nothing is offered from this place.
    None,
    /// A profile that can be used.
    Relay(Box<Relay>),
    /// A profile that cannot be used, with the file and what is wrong with
    /// it. Found before any connection (REQ-N-015).
    Unusable(ProfileError),
}

impl Lookup {
    fn of(found: Result<Option<(Source, RelayProfile)>, ProfileError>) -> Self {
        match found {
            Ok(None) => Self::None,
            Ok(Some((source, profile))) => match Relay::checked(source, profile) {
                Ok(relay) => Self::Relay(Box::new(relay)),
                Err(error) => Self::Unusable(error),
            },
            Err(error) => Self::Unusable(error),
        }
    }

    /// The relay, when the profile can be used.
    pub(crate) fn relay(&self) -> Option<&Relay> {
        match self {
            Self::Relay(relay) => Some(relay),
            Self::None | Self::Unusable(_) => None,
        }
    }
}

/// Reads the profile file next to the program `exe`: the one place both roles
/// read, and the only one the customer role reads. Nothing is written.
pub(crate) fn next_to_program(exe: &Path) -> Lookup {
    Lookup::of(
        relay_profile::read_next_to(exe)
            .map(|found| found.map(|(path, profile)| (Source::File(path), profile))),
    )
}

/// Reads the relay of the installed copy's settings. Nothing is written.
pub(crate) fn in_settings(layout: &Layout) -> Lookup {
    Lookup::of(settings::read(layout).map(|settings| {
        settings
            .relay
            .map(|profile| (Source::Settings(layout.settings.clone()), profile))
    }))
}

/// This process as far as its relay is concerned: the program file, and the
/// install it runs from when it is the installed copy.
#[derive(Debug, Clone)]
pub(crate) struct ThisCopy {
    /// The running program.
    pub(crate) exe: PathBuf,
    /// The layout of the install, only when this copy is the installed one:
    /// the one copy that keeps settings. A copy that runs once has none and
    /// writes nothing but its audit trail.
    pub(crate) installed: Option<Layout>,
}

impl ThisCopy {
    /// This process. `None` when it cannot find its own program file.
    pub(crate) fn current() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        let installed = Layout::current().filter(|layout| layout.is_installed_exe(&exe));
        Some(Self { exe, installed })
    }
}

/// The relays a technician copy knows: the file next to the program, and the
/// settings when it is the installed copy.
#[derive(Debug, Clone)]
pub(crate) struct TechnicianRelays {
    /// Where the profile file next to the program is looked for.
    pub(crate) file_path: Option<PathBuf>,
    /// What that file says.
    pub(crate) file: Lookup,
    /// What the settings say; `None` for a copy that keeps no settings.
    pub(crate) stored: Option<Lookup>,
}

impl TechnicianRelays {
    /// Reads both places. Nothing is written.
    pub(crate) fn read(copy: &ThisCopy) -> Self {
        Self {
            file_path: relay_profile::path_next_to(&copy.exe),
            file: next_to_program(&copy.exe),
            stored: copy.installed.as_ref().map(in_settings),
        }
    }

    /// The profile this copy uses: the file next to the program when it has
    /// one, also when that one cannot be used, and the settings otherwise. A
    /// file that is wrong is never passed over for another relay in silence.
    pub(crate) fn active(&self) -> &Lookup {
        match (&self.file, &self.stored) {
            (Lookup::None, Some(stored)) => stored,
            (file, _) => file,
        }
    }
}

// --- What a screen shows ---

/// A relay as a screen shows it. It never holds the key, only whether there
/// is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RelayView {
    /// The relay's host name: the name its certificate must carry.
    pub(crate) host: String,
    /// Where it is reached: `host:port`.
    pub(crate) address: String,
    /// Whether the profile holds an admission key.
    pub(crate) key_set: bool,
    /// The certificate authority file the profile names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ca: Option<String>,
    /// The name in the certificate, when the profile gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

impl RelayView {
    pub(crate) fn of(profile: &RelayProfile) -> Self {
        Self {
            host: profile.server_name().to_owned(),
            address: profile.address().to_string(),
            key_set: profile.key().is_some(),
            ca: profile.ca().map(str::to_owned),
            name: profile.name().map(str::to_owned),
        }
    }
}

/// A profile that cannot be used, for a screen: the file, the setting, and
/// what is wrong as a sentence. It never holds text from the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProblemView {
    /// The file that was read.
    pub(crate) file: String,
    /// The setting concerned, as the file names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) field: Option<&'static str>,
    /// What is wrong, as a sentence.
    pub(crate) message: String,
}

impl ProblemView {
    pub(crate) fn of(error: &ProfileError) -> Self {
        Self {
            file: error.file.display().to_string(),
            field: error.problem.field.map(Field::name),
            message: sentence(&error.problem.to_string()),
        }
    }
}

/// A text as a sentence: a capital first, a full stop last.
pub(crate) fn sentence(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 1);
    let mut chars = text.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// What one place says about a relay, for a screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaceView {
    /// The file of this place.
    pub(crate) path: String,
    /// The relay it holds, when it can be used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) relay: Option<RelayView>,
    /// What is wrong with it, when it cannot be used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) problem: Option<ProblemView>,
}

impl PlaceView {
    pub(crate) fn of(path: &Path, lookup: &Lookup) -> Self {
        Self {
            path: path.display().to_string(),
            relay: lookup.relay().map(Relay::view),
            problem: match lookup {
                Lookup::Unusable(error) => Some(ProblemView::of(error)),
                Lookup::None | Lookup::Relay(_) => None,
            },
        }
    }
}

// --- Reaching the relay ---

/// The marker inside the I/O error of a relay whose name has no address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NameNotFound;

impl fmt::Display for NameNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the name of the relay was not found")
    }
}

impl std::error::Error for NameNotFound {}

/// A dial to the relay of a profile: it finds the addresses of the relay's
/// host, connects to the first that takes the connection, and speaks `wss`
/// with the certificate checked against the relay's name ([`WssRelayDial`]).
///
/// The name is looked up for every dial, so a relay that moved is found again
/// by a side that waits for minutes. A certificate that is not trusted, a
/// server that does not answer as a relay and a refusal end the dial at
/// once: only an address that takes no connection lets the next one be tried,
/// and nothing ever falls back to a connection without TLS.
pub(crate) struct EndpointDial {
    host: String,
    port: u16,
    server_name: String,
    connector: WssConnector,
    ticket: RendezvousTicket,
    token: Option<AdmissionToken>,
}

/// The ticket and the key are not in it.
impl fmt::Debug for EndpointDial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointDial")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("server_name", &self.server_name)
            .field("has_token", &self.token.is_some())
            .finish()
    }
}

impl EndpointDial {
    /// The addresses of the relay's host, in the order the system gives them.
    async fn addresses(&self) -> Result<Vec<SocketAddr>, RelayFailure> {
        if let Ok(address) = self.host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(address, self.port)]);
        }
        let not_found =
            || RelayFailure::Unreachable(io::Error::new(io::ErrorKind::NotFound, NameNotFound));
        let lookup = tokio::time::timeout(
            RELAY_CONNECT_TIMEOUT,
            tokio::net::lookup_host((self.host.as_str(), self.port)),
        )
        .await
        .map_err(|_elapsed| {
            RelayFailure::Unreachable(io::Error::new(
                io::ErrorKind::TimedOut,
                "the name of the relay was not answered in time",
            ))
        })?
        .map_err(|_| not_found())?;
        let addresses: Vec<SocketAddr> = lookup.take(MAX_ADDRESSES).collect();
        if addresses.is_empty() {
            return Err(not_found());
        }
        Ok(addresses)
    }
}

impl Dial for EndpointDial {
    type Stream = <WssRelayDial as Dial>::Stream;

    async fn dial(&self) -> Result<Self::Stream, SessionError> {
        let mut unreachable = None;
        for address in self.addresses().await? {
            let dial = WssRelayDial::new(
                address,
                self.server_name.clone(),
                self.connector.clone(),
                self.ticket.clone(),
                self.token.clone(),
            );
            match dial.dial().await {
                // Nothing listens on this address: the name may have another.
                Err(SessionError::Relay(RelayFailure::Unreachable(error))) => {
                    unreachable = Some(error);
                }
                other => return other,
            }
        }
        Err(RelayFailure::Unreachable(
            unreachable.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, NameNotFound)),
        )
        .into())
    }
}

// --- Words ---

/// Who reads a message: the technician, who can change the relay and its
/// settings, or the customer, who can only tell the technician.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Audience {
    /// The technician role.
    Technician,
    /// The customer role.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the customer role reaches a relay from ST-M007-013 on"
        )
    )]
    Customer,
}

/// Why a relay could not be used, in plain words with a next step
/// (REQ-N-008, REQ-N-015). No text here ever holds the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FailureWords {
    /// A stable name of the failure: `unreachable`, `name_not_found`,
    /// `untrusted`, `no_answer`, `key_required`, `key_refused`,
    /// `rate_limited`, `busy`, `unavailable`, `version` or `settings`.
    pub(crate) kind: &'static str,
    /// What happened.
    pub(crate) message: String,
    /// What the reader can do next.
    pub(crate) next: String,
}

/// Words for a relay that could not be used.
pub(crate) fn failure_words(
    failure: &RelayFailure,
    relay: &RelayView,
    audience: Audience,
) -> FailureWords {
    let host = &relay.host;
    let address = &relay.address;
    let technician = audience == Audience::Technician;
    // What the customer does about anything that only the technician can
    // put right.
    let tell = |what: &str| {
        format!("Tell your technician that {what}. Nobody has access to this computer.")
    };
    let words = |kind: &'static str, message: String, next: String| FailureWords {
        kind,
        message,
        next,
    };
    match failure {
        RelayFailure::Unreachable(error) if name_not_found(error) => words(
            "name_not_found",
            format!(
                "The name of the relay in {address} was not found. The address may be mistyped, \
                 or this computer is not online."
            ),
            if technician {
                "Check the address of the relay and that this computer is online, then try again."
                    .to_owned()
            } else {
                format!(
                    "Check that this computer is online, then try again. If it stays like this, {}",
                    lower_first(&tell(&format!("the relay {host} was not found")))
                )
            },
        ),
        RelayFailure::Unreachable(_) => words(
            "unreachable",
            format!(
                "CretLink could not connect to the relay at {address}. The relay may be off, the \
                 address may be wrong, or this network does not let this computer reach it."
            ),
            if technician {
                "Check that the relay is running and that its address and port are right, then \
                 try again."
                    .to_owned()
            } else {
                format!(
                    "Check that this computer is online, then try again. If it stays like this, {}",
                    lower_first(&tell(&format!("the relay {host} cannot be reached")))
                )
            },
        ),
        RelayFailure::Untrusted(_) => words(
            "untrusted",
            format!(
                "The relay at {address} showed no certificate that this computer trusts for the \
                 name {host}, so nothing was sent to it. The certificate may come from an \
                 authority this computer does not know, be out of date or carry another name; or \
                 the server at that address is not the relay."
            ),
            if !technician {
                tell(&format!(
                    "this computer does not trust the certificate of the relay {host}"
                ))
            } else if relay.ca.is_some() {
                "Check that the certificate file belongs to the authority that signed the relay's \
                 certificate, and that the name is the one in that certificate."
                    .to_owned()
            } else {
                "If the relay's certificate comes from an authority of your own, give its \
                 certificate file. If it carries another name than the host of the address, give \
                 that name."
                    .to_owned()
            },
        ),
        RelayFailure::NoAnswer(_) => words(
            "no_answer",
            format!(
                "The server at {address} did not answer as a CretLink relay does. It may be \
                 another service, or a relay server of an older release."
            ),
            if technician {
                "Check the address and the port, and that the relay server comes from the same \
                 release as this copy of CretLink."
                    .to_owned()
            } else {
                tell(&format!("the relay {host} did not answer"))
            },
        ),
        RelayFailure::Refused(RelayRefusal::CredentialRequired) => words(
            "key_required",
            format!("The relay {host} asks for an admission key, and this copy has none."),
            if technician {
                "Enter the key that the relay's operator gave this copy.".to_owned()
            } else {
                "Ask your technician for a copy of CretLink that holds a key for their relay."
                    .to_owned()
            },
        ),
        RelayFailure::Refused(RelayRefusal::CredentialRejected) => words(
            "key_refused",
            format!(
                "The relay {host} does not know the admission key of this copy. The key may be \
                 mistyped, or it was withdrawn."
            ),
            if technician {
                "Enter the key again, or ask the relay's operator for a new one.".to_owned()
            } else {
                "Ask your technician for a new copy of CretLink, or to look at the keys of their \
                 relay."
                    .to_owned()
            },
        ),
        RelayFailure::Refused(RelayRefusal::RateLimited) => words(
            "rate_limited",
            format!("The relay {host} had too many attempts in a short time."),
            "Wait a minute, then try again.".to_owned(),
        ),
        RelayFailure::Refused(RelayRefusal::Busy) => words(
            "busy",
            format!("The relay {host} has no free place right now."),
            if technician {
                "Try again in a few minutes. If it stays full, look at the limits of the relay \
                 and at who uses it."
                    .to_owned()
            } else {
                "Try again in a few minutes.".to_owned()
            },
        ),
        RelayFailure::Refused(RelayRefusal::Unavailable) => words(
            "unavailable",
            format!("The relay {host} cannot admit connections right now."),
            if technician {
                "Look at the log of the relay server: it needs its operator.".to_owned()
            } else {
                tell(&format!("the relay {host} admits nobody right now"))
            },
        ),
        RelayFailure::Refused(RelayRefusal::UnsupportedVersion { relay: version }) => words(
            "version",
            format!(
                "The relay {host} and this copy of CretLink come from releases that do not fit \
                 together: the relay speaks version {version} of the rendezvous."
            ),
            if technician {
                "Update the relay server and CretLink to the same release.".to_owned()
            } else {
                tell(&format!(
                    "the relay {host} and this copy of CretLink do not fit together"
                ))
            },
        ),
        RelayFailure::Settings(detail) => words(
            "settings",
            format!(
                "The relay settings of this copy cannot be used: {}",
                sentence(detail)
            ),
            if technician {
                "Correct the relay settings, then try again.".to_owned()
            } else {
                tell("the relay settings of this copy cannot be used")
            },
        ),
    }
}

/// Whether an I/O error says that the relay's name has no address.
fn name_not_found(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<NameNotFound>())
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What reaching the relay once came to, in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TestResult {
    /// Whether the relay answered and admits this copy.
    pub(crate) ok: bool,
    /// `admitted`, or the name of the failure ([`FailureWords::kind`]).
    pub(crate) kind: &'static str,
    /// What happened.
    pub(crate) message: String,
    /// What to do next; empty when the relay admits this copy.
    pub(crate) next: String,
}

impl TestResult {
    fn admitted(relay: &RelayView, pinned: bool) -> Self {
        let checked = if pinned {
            "the certificate file of this profile"
        } else {
            "the certificate authorities of this computer"
        };
        Self {
            ok: true,
            kind: "admitted",
            message: format!(
                "The relay {host} answered and admits this copy. Its certificate is valid for \
                 {host}, checked against {checked}.",
                host = relay.host
            ),
            next: String::new(),
        }
    }

    pub(crate) fn failed(words: FailureWords) -> Self {
        Self {
            ok: false,
            kind: words.kind,
            message: words.message,
            next: words.next,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
