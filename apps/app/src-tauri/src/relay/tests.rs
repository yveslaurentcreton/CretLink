//! Tests of the relay of a copy (ST-M007-011): where its profile comes from,
//! what is checked before any connection, and what reaching the relay says.
//!
//! The relay is the real relay server with TLS inside the test process
//! ([`testing::TestRelay`]); nothing here leaves the loopback interface.

use std::time::Duration;

use cretlink_cli::settings::{self, Settings};
use cretlink_relay::{AdmissionPolicy, ClientId};
use tokio::net::TcpListener;

use super::testing::{CA_FILE, Folder, Home, RELAY_NAME, TestRelay};
use super::*;

/// A key as the relay's operator gives a copy.
const KEY: &str = "5f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5";

/// How long a step against the relay inside this process may take.
const STEP: Duration = Duration::from_secs(20);

fn profile(text: &str) -> RelayProfile {
    relay_profile::parse(text).unwrap().unwrap()
}

fn store(home: &Home, text: &str) {
    settings::write(
        &home.layout,
        &Settings {
            relay: Some(profile(text)),
            network_adapter: None,
        },
    )
    .unwrap();
}

const SAM: &str = "[relay]\naddress = \"relay.example.org:443\"\n";
const OTHER: &str = "[relay]\naddress = \"host-01.example.org:9970\"\n";

// --- Where the profile comes from ---

#[test]
fn without_a_profile_no_relay_is_offered() {
    let home = Home::new("none");
    for copy in [home.installed(), home.run_once()] {
        let relays = TechnicianRelays::read(&copy);
        assert!(matches!(relays.active(), Lookup::None), "{copy:?}");
        assert!(relays.active().relay().is_none());
    }
    // The customer role: the file next to its program, and there is none.
    assert!(matches!(
        next_to_program(&home.folder.path("CretLink-Support.exe")),
        Lookup::None
    ));
    // Looking wrote nothing.
    assert!(home.folder.snapshot().is_empty());
}

#[test]
fn a_file_next_to_the_program_is_read_by_either_role() {
    let folder = Folder::new("next-to");
    let file = folder.write("cretlink.toml", format!("{SAM}key = \"{KEY}\"\n"));
    for program in ["cretlink", "CretLink-Support.exe"] {
        let lookup = next_to_program(&folder.path(program));
        let relay = lookup.relay().expect("the profile next to the program");
        assert_eq!(relay.host(), "relay.example.org");
        assert_eq!(relay.source, Source::File(file.clone()));
        assert_eq!(
            relay.view(),
            RelayView {
                host: "relay.example.org".to_owned(),
                address: "relay.example.org:443".to_owned(),
                key_set: true,
                ca: None,
                name: None,
            }
        );
    }
}

#[test]
fn a_file_next_to_the_program_wins_over_the_settings() {
    let home = Home::new("file-wins");
    store(&home, SAM);
    let copy = home.installed();
    let relays = TechnicianRelays::read(&copy);
    assert_eq!(
        relays.active().relay().unwrap().source,
        Source::Settings(home.layout.settings.clone())
    );

    let file = home
        .folder
        .write(relays.file_path.as_ref().unwrap().to_str().unwrap(), OTHER);
    let relays = TechnicianRelays::read(&copy);
    let active = relays.active().relay().unwrap();
    assert_eq!(active.source, Source::File(file));
    assert_eq!(active.host(), "host-01.example.org");
    // What the settings hold is still there, and not in use.
    assert_eq!(
        relays.stored.as_ref().unwrap().relay().unwrap().host(),
        "relay.example.org"
    );
}

#[test]
fn a_file_that_cannot_be_used_is_never_passed_over_for_the_settings() {
    let home = Home::new("file-wrong");
    store(&home, SAM);
    let copy = home.installed();
    let path = relay_profile::path_next_to(&copy.exe).unwrap();
    home.folder.write(
        path.to_str().unwrap(),
        "[relay]\naddress = \"host-01.example.org\"\n",
    );
    let relays = TechnicianRelays::read(&copy);
    let Lookup::Unusable(error) = relays.active() else {
        panic!("expected the file's problem, got {:?}", relays.active());
    };
    assert_eq!(error.file, path);
    let view = ProblemView::of(error);
    assert_eq!(view.field, Some("address"));
    assert_eq!(
        view.message,
        "Line 2: `address` needs a port; write the relay's host name and port, like \
         relay.example.org:443."
    );
    assert_eq!(view.file, path.display().to_string());
}

#[test]
fn a_file_without_a_relay_leaves_the_settings_in_use() {
    let home = Home::new("file-empty");
    store(&home, SAM);
    let copy = home.installed();
    let path = relay_profile::path_next_to(&copy.exe).unwrap();
    home.folder
        .write(path.to_str().unwrap(), "# no relay in this file\n");
    let relays = TechnicianRelays::read(&copy);
    assert_eq!(relays.active().relay().unwrap().host(), "relay.example.org");
}

#[test]
fn only_the_installed_copy_reads_the_settings() {
    let home = Home::new("run-once");
    store(&home, SAM);
    // A copy that runs once on the same computer does not use them.
    let relays = TechnicianRelays::read(&home.run_once());
    assert!(relays.stored.is_none());
    assert!(matches!(relays.active(), Lookup::None));
    // Neither does the customer role, also when it is the installed program
    // that runs it: it reads the profile file next to the program only.
    for program in [
        home.layout.exe.clone(),
        home.folder.path("CretLink-Support.exe"),
    ] {
        assert!(matches!(next_to_program(&program), Lookup::None));
    }
}

#[test]
fn settings_that_cannot_be_read_are_a_problem_of_the_settings_file() {
    let home = Home::new("settings-wrong");
    home.folder.write(
        home.layout.settings.to_str().unwrap(),
        format!("[relay]\naddress = \"relay.example.org:443\"\nkey = {KEY}\n"),
    );
    let relays = TechnicianRelays::read(&home.installed());
    let Lookup::Unusable(error) = relays.active() else {
        panic!("expected a problem, got {:?}", relays.active());
    };
    assert_eq!(error.file, home.layout.settings);
    let shown = format!("{:?} {}", ProblemView::of(error), error);
    assert!(!shown.contains(KEY), "{shown}");
}

// --- The certificate authority file ---

#[tokio::test]
async fn the_certificate_file_is_checked_before_any_connection() {
    let relay = TestRelay::start(AdmissionPolicy::Open).await;
    let folder = Folder::new("ca");
    let program = relay.set_up(&folder, None);
    let found = next_to_program(&program);
    let found = found.relay().expect("a profile with its certificate file");
    assert!(found.pinned.is_some());
    assert_eq!(found.view().ca.as_deref(), Some(CA_FILE));
    assert_eq!(found.host(), RELAY_NAME);

    // No file of that name next to the profile.
    std::fs::remove_file(folder.path(CA_FILE)).unwrap();
    let Lookup::Unusable(error) = next_to_program(&program) else {
        panic!("a missing certificate file is a problem of the profile");
    };
    assert_eq!(error.problem.field, Some(Field::Ca));
    assert!(error.problem.text.contains("not in the folder"));

    // A file that looks like a certificate and is none.
    folder.write(
        CA_FILE,
        "-----BEGIN CERTIFICATE-----\nbm90IGEgY2VydGlmaWNhdGU=\n-----END CERTIFICATE-----\n",
    );
    let Lookup::Unusable(error) = next_to_program(&program) else {
        panic!("a certificate that cannot be used is a problem of the profile");
    };
    assert_eq!(error.problem.field, Some(Field::Ca));
    assert_eq!(
        ProblemView::of(&error).message,
        "The file that `ca` names holds no certificate that can be used."
    );
}

// --- Reaching the relay ---

async fn test_of(program: &std::path::Path, audience: Audience) -> TestResult {
    let lookup = next_to_program(program);
    let relay = lookup.relay().expect("a profile that can be used");
    tokio::time::timeout(STEP, relay.test(audience))
        .await
        .expect("the test of the relay ends")
}

#[tokio::test]
async fn the_test_says_that_the_relay_admits_this_copy() {
    let relay = TestRelay::start(AdmissionPolicy::Open).await;
    let folder = Folder::new("admitted");
    let program = relay.set_up(&folder, None);
    let result = test_of(&program, Audience::Technician).await;
    assert!(result.ok, "{result:?}");
    assert_eq!(result.kind, "admitted");
    assert_eq!(
        result.message,
        "The relay relay.test answered and admits this copy. Its certificate is valid for \
         relay.test, checked against the certificate file of this profile."
    );
    assert_eq!(result.next, "");
    // The test met nobody: the relay forwarded nothing.
    let metrics = relay.control.metrics();
    assert_eq!(metrics.sessions_admitted, 1, "{metrics:?}");
    assert_eq!((metrics.frames_forwarded, metrics.bytes_forwarded), (0, 0));
}

#[tokio::test]
async fn the_test_tells_a_missing_key_from_a_key_the_relay_does_not_know() {
    let policy = AdmissionPolicy::Allowlist(vec![(
        AdmissionToken::from_text(KEY),
        ClientId::new("sam-laptop"),
    )]);
    let relay = TestRelay::start(policy).await;

    let folder = Folder::new("key");
    let program = relay.set_up(&folder, Some(KEY));
    assert!(test_of(&program, Audience::Technician).await.ok);

    let program = relay.set_up(&folder, None);
    let missing = test_of(&program, Audience::Technician).await;
    assert!(!missing.ok);
    assert_eq!(missing.kind, "key_required");
    assert_eq!(
        missing.message,
        "The relay relay.test asks for an admission key, and this copy has none."
    );
    assert_eq!(
        missing.next,
        "Enter the key that the relay's operator gave this copy."
    );

    let wrong = "0000000000000000-not-the-key-of-this-copy";
    let program = relay.set_up(&folder, Some(wrong));
    let refused = test_of(&program, Audience::Customer).await;
    assert_eq!(refused.kind, "key_refused");
    assert!(refused.message.contains("does not know the admission key"));
    assert!(
        refused.next.starts_with("Ask your technician"),
        "{}",
        refused.next
    );
    // No word of it repeats the key.
    let shown = format!("{refused:?}");
    assert!(!shown.contains(wrong) && !shown.contains(KEY), "{shown}");
}

#[tokio::test]
async fn a_certificate_that_is_not_trusted_ends_the_test_and_nothing_is_sent() {
    let relay = TestRelay::start(AdmissionPolicy::Open).await;
    let folder = Folder::new("untrusted");

    // The certificate file of another relay.
    let other = TestRelay::start(AdmissionPolicy::Open).await;
    relay.set_up(&folder, None);
    let program = folder.path("CretLink-Support.exe");
    folder.write(CA_FILE, &other.cert_pem);
    let result = test_of(&program, Audience::Technician).await;
    assert_eq!(result.kind, "untrusted");
    assert!(
        result
            .message
            .contains("showed no certificate that this computer trusts")
    );
    assert!(
        result.message.contains("nothing was sent"),
        "{}",
        result.message
    );
    assert!(
        result
            .next
            .contains("certificate file belongs to the authority")
    );

    // The right certificate under another name.
    folder.write(CA_FILE, &relay.cert_pem);
    folder.write(
        "cretlink.toml",
        format!(
            "[relay]\naddress = \"{}\"\nname = \"other.test\"\nca = \"{CA_FILE}\"\n",
            relay.addr
        ),
    );
    let result = test_of(&program, Audience::Customer).await;
    assert_eq!(result.kind, "untrusted");
    assert!(
        result.message.contains("for the name other.test"),
        "{}",
        result.message
    );
    assert!(
        result.next.starts_with("Tell your technician"),
        "{}",
        result.next
    );

    // Without a certificate file the system's authorities decide, and they do
    // not know a certificate that was minted for this test.
    folder.write(
        "cretlink.toml",
        format!(
            "[relay]\naddress = \"{}\"\nname = \"{RELAY_NAME}\"\n",
            relay.addr
        ),
    );
    let result = test_of(&program, Audience::Technician).await;
    assert_eq!(result.kind, "untrusted", "{result:?}");
    assert!(
        result.next.contains("give its certificate file"),
        "{}",
        result.next
    );

    // The relay never saw a rendezvous: every attempt ended before one.
    let metrics = relay.control.metrics();
    assert_eq!(metrics.sessions_admitted, 0, "{metrics:?}");
}

#[tokio::test]
async fn a_relay_that_is_not_there_is_said_as_unreachable() {
    // A port that nothing listens on: one that was just given back.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let relay = TestRelay::start(AdmissionPolicy::Open).await;
    let folder = Folder::new("unreachable");
    relay.set_up(&folder, None);
    let program = folder.path("cretlink");
    folder.write(
        "cretlink.toml",
        format!("[relay]\naddress = \"{addr}\"\nname = \"{RELAY_NAME}\"\nca = \"{CA_FILE}\"\n"),
    );
    let result = test_of(&program, Audience::Technician).await;
    assert_eq!(result.kind, "unreachable");
    assert_eq!(
        result.message,
        format!(
            "CretLink could not connect to the relay at {addr}. The relay may be off, the \
             address may be wrong, or this network does not let this computer reach it."
        )
    );
    assert!(result.next.starts_with("Check that the relay is running"));
}

#[tokio::test]
async fn a_server_that_is_no_relay_is_said_as_one_that_did_not_answer() {
    // A server with the relay's certificate that takes the connection, speaks
    // TLS and WebSocket, and then hangs up without an answer.
    let (cert, key) = cretlink_wss::testcert::self_signed(RELAY_NAME).unwrap();
    let cert_pem = super::testing::pem(cert.as_ref());
    let acceptor = cretlink_wss::acceptor(cretlink_wss::server_config(vec![cert], key).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            drop(cretlink_wss::accept(&acceptor, tcp).await);
        }
    });

    let folder = Folder::new("no-answer");
    folder.write(CA_FILE, cert_pem);
    folder.write(
        "cretlink.toml",
        format!("[relay]\naddress = \"{addr}\"\nname = \"{RELAY_NAME}\"\nca = \"{CA_FILE}\"\n"),
    );
    let result = test_of(&folder.path("cretlink"), Audience::Technician).await;
    assert_eq!(result.kind, "no_answer", "{result:?}");
    assert!(
        result
            .message
            .contains("did not answer as a CretLink relay does")
    );
    assert!(result.next.contains("same release"), "{}", result.next);
    server.abort();
}

#[tokio::test]
async fn a_relay_is_found_by_its_name_and_the_next_address_is_tried() {
    // `localhost` is a name this computer answers itself, with one address or
    // two. The relay listens on the IPv4 one only, so where an IPv6 address
    // comes first, nothing listens there and the next one is taken.
    let relay = TestRelay::start(AdmissionPolicy::Open).await;
    let folder = Folder::new("by-name");
    relay.set_up(&folder, None);
    folder.write(
        "cretlink.toml",
        format!(
            "[relay]\naddress = \"localhost:{}\"\nname = \"{RELAY_NAME}\"\nca = \"{CA_FILE}\"\n",
            relay.addr.port()
        ),
    );
    let program = folder.path("cretlink");
    let lookup = next_to_program(&program);
    let found = lookup.relay().unwrap();
    // The certificate carries the relay's name, not the name it was reached by.
    assert_eq!(found.host(), RELAY_NAME);
    assert_eq!(
        found.view().address,
        format!("localhost:{}", relay.addr.port())
    );
    let result = test_of(&program, Audience::Technician).await;
    assert!(result.ok, "{result:?}");
}

#[tokio::test]
async fn an_address_is_dialled_as_it_stands_and_a_name_without_one_is_not_found() {
    let connector = WssRelayDial::connector(Some(
        TestRelay::start(AdmissionPolicy::Open)
            .await
            .cert_pem
            .as_bytes(),
    ))
    .unwrap();
    let dial = |host: &str| EndpointDial {
        host: host.to_owned(),
        port: 443,
        server_name: RELAY_NAME.to_owned(),
        connector: connector.clone(),
        ticket: RendezvousTicket::from_bytes([7; 16]),
        token: Some(AdmissionToken::from_text(KEY)),
    };
    assert_eq!(
        dial("192.0.2.1").addresses().await.unwrap(),
        vec!["192.0.2.1:443".parse().unwrap()]
    );
    assert_eq!(
        dial("2001:db8::1").addresses().await.unwrap(),
        vec!["[2001:db8::1]:443".parse().unwrap()]
    );
    // Neither the ticket nor the key is printed.
    let shown = format!("{:?}", dial("192.0.2.1"));
    assert!(shown.contains("has_token: true"), "{shown}");
    assert!(!shown.contains(KEY) && !shown.contains("ticket"), "{shown}");
}

// --- Words ---

fn sample() -> RelayView {
    RelayView {
        host: "relay.example.org".to_owned(),
        address: "relay.example.org:443".to_owned(),
        key_set: true,
        ca: None,
        name: None,
    }
}

fn every_failure() -> Vec<RelayFailure> {
    let io = || io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
    vec![
        RelayFailure::Unreachable(io()),
        RelayFailure::Unreachable(io::Error::new(io::ErrorKind::NotFound, NameNotFound)),
        RelayFailure::Untrusted(io()),
        RelayFailure::NoAnswer(io()),
        RelayFailure::Refused(RelayRefusal::CredentialRequired),
        RelayFailure::Refused(RelayRefusal::CredentialRejected),
        RelayFailure::Refused(RelayRefusal::RateLimited),
        RelayFailure::Refused(RelayRefusal::Busy),
        RelayFailure::Refused(RelayRefusal::Unavailable),
        RelayFailure::Refused(RelayRefusal::UnsupportedVersion { relay: 1 }),
        RelayFailure::Settings("no usable system trust anchors were found".to_owned()),
    ]
}

#[test]
fn every_failure_has_words_of_its_own_and_a_next_step() {
    for audience in [Audience::Technician, Audience::Customer] {
        let mut kinds = Vec::new();
        let mut messages = Vec::new();
        for failure in every_failure() {
            let words = failure_words(&failure, &sample(), audience);
            assert!(!kinds.contains(&words.kind), "{} twice", words.kind);
            assert!(!messages.contains(&words.message), "{}", words.message);
            for text in [&words.message, &words.next] {
                assert!(text.ends_with('.'), "{text:?}");
                assert!(text.chars().next().unwrap().is_uppercase(), "{text:?}");
                assert!(!text.contains("  "), "{text:?}");
            }
            // The relay is named, by its host or its address.
            assert!(
                words.message.contains("relay.example.org") || words.kind == "settings",
                "{}",
                words.message
            );
            kinds.push(words.kind);
            messages.push(words.message);
        }
        assert_eq!(
            kinds,
            [
                "unreachable",
                "name_not_found",
                "untrusted",
                "no_answer",
                "key_required",
                "key_refused",
                "rate_limited",
                "busy",
                "unavailable",
                "version",
                "settings"
            ]
        );
    }
}

#[test]
fn the_customer_is_told_what_to_pass_on_and_the_technician_what_to_change() {
    let untrusted = RelayFailure::Untrusted(io::Error::other("bad certificate"));
    let technician = failure_words(&untrusted, &sample(), Audience::Technician);
    let customer = failure_words(&untrusted, &sample(), Audience::Customer);
    assert_eq!(technician.message, customer.message);
    assert!(technician.next.contains("give its certificate file"));
    assert_eq!(
        customer.next,
        "Tell your technician that this computer does not trust the certificate of the relay \
         relay.example.org. Nobody has access to this computer."
    );
    // With a certificate file in the profile the technician is pointed at it.
    let pinned = RelayView {
        ca: Some(CA_FILE.to_owned()),
        ..sample()
    };
    assert!(
        failure_words(&untrusted, &pinned, Audience::Technician)
            .next
            .contains("certificate file belongs to the authority")
    );

    let version = RelayFailure::Refused(RelayRefusal::UnsupportedVersion { relay: 3 });
    assert!(
        failure_words(&version, &sample(), Audience::Technician)
            .message
            .contains("version 3 of the rendezvous")
    );
    let lost = RelayFailure::Unreachable(io::Error::new(io::ErrorKind::NotFound, NameNotFound));
    let words = failure_words(&lost, &sample(), Audience::Customer);
    assert_eq!(
        words.next,
        "Check that this computer is online, then try again. If it stays like this, tell your \
         technician that the relay relay.example.org was not found. Nobody has access to this \
         computer."
    );
}

// --- What never leaves this side ---

#[test]
fn no_view_and_no_debug_output_holds_the_key() {
    let folder = Folder::new("redacted");
    folder.write("cretlink.toml", format!("{SAM}key = \"{KEY}\"\n"));
    let lookup = next_to_program(&folder.path("cretlink"));
    let relay = lookup.relay().unwrap();
    let place = PlaceView::of(&folder.path("cretlink.toml"), &lookup);
    let shown = format!(
        "{relay:?} {lookup:?} {:?} {}",
        relay.view(),
        serde_json::to_string(&place).unwrap()
    );
    assert!(!shown.contains(KEY), "{shown}");
    assert_eq!(
        serde_json::to_value(&place).unwrap(),
        serde_json::json!({
            "path": folder.path("cretlink.toml").display().to_string(),
            "relay": {
                "host": "relay.example.org",
                "address": "relay.example.org:443",
                "keySet": true,
            },
        })
    );
    // The key is there for the one place it goes: the rendezvous preamble.
    assert_eq!(relay.profile().key().unwrap().as_bytes(), KEY.as_bytes());
}

#[test]
fn a_text_becomes_a_sentence() {
    assert_eq!(
        sentence("line 2: `address` needs a port"),
        "Line 2: `address` needs a port."
    );
    assert_eq!(sentence("Already one."), "Already one.");
}

#[test]
fn a_lost_connection_is_named_with_the_relay_only_when_the_session_ran_through_one() {
    use cretlink_session::{EndedBy, SessionEvent};

    let gone = || SessionEvent::Failed {
        stage: "consent".to_owned(),
        message: "peer disconnected".to_owned(),
    };
    // Through a relay, and the runtime says the connection was lost.
    assert_eq!(
        name_lost_connection(
            gone(),
            Some(EndedBy::ConnectionLost),
            Some("relay.example.org")
        ),
        SessionEvent::Failed {
            stage: "relay_lost".to_owned(),
            message: "The connection through the relay relay.example.org was lost.".to_owned(),
        }
    );
    // On one network the failure is what it was.
    assert_eq!(
        name_lost_connection(gone(), Some(EndedBy::ConnectionLost), None),
        gone()
    );
    // A failure that is no lost connection keeps its stage: a mismatch of
    // the approval code through a relay is still a mismatch.
    for ended_by in [
        Some(EndedBy::Failure),
        Some(EndedBy::Technician),
        Some(EndedBy::Customer),
        None,
    ] {
        assert_eq!(
            name_lost_connection(gone(), ended_by, Some("relay.example.org")),
            gone()
        );
    }
    // Only a failure is renamed; a revoke stays a revoke.
    let revoked = SessionEvent::Revoked {
        reason: cretlink_session::RevokeReason::Error,
    };
    assert_eq!(
        name_lost_connection(
            revoked.clone(),
            Some(EndedBy::ConnectionLost),
            Some("relay.example.org")
        ),
        revoked
    );
}
