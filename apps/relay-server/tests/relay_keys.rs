//! One admission key per customer copy and technician (ST-M007-014).
//!
//! These tests run the **real** relay server with a key file: every key is
//! admitted under its own label, a key that the operator withdraws is refused
//! for new legs while a session in progress continues, a changed file that
//! breaks a rule changes nothing, and a relay without a free place says so.
//! The last tests run the released program itself for what it refuses at
//! startup and for `keygen`.

// `unwrap`/`expect`/`panic` are the assertion vocabulary of the test suites.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use cretlink_relay::{
    AdmissionControl, AdmissionPolicy, AdmissionToken, ClientId, ControlConfig, OpEventKind,
    Preamble, RENDEZVOUS_TICKET_LEN, RelayAnswer, RelayRefusal, Rendezvous, RendezvousTicket,
    StopSignal, read_answer, write_preamble,
};
use cretlink_relay_server::keys::{load_keys, watch_keys};
use cretlink_relay_server::{Leg, ServeConfig, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// Per-step wait budget; everything here is local and takes milliseconds.
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the relay of these tests looks at its key file.
const POLL: Duration = Duration::from_millis(25);

const KEY_ACME: &str = "acme-0123456789abcdef0123456789abcdef";
const KEY_FRONTDESK: &str = "frontdesk-fedcba9876543210fedcba98765";
const KEY_NEWCOMER: &str = "newcomer-00112233445566778899aabbccdd";

/// A directory of its own for one test, removed when the value is dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("cretlink-relay-keys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes a key file with one `label:key` line per entry.
fn write_keys(path: &Path, entries: &[(&str, &str)]) {
    let mut text = String::from("# admission keys of the test relay\n");
    for (label, key) in entries {
        text.push_str(&format!("{label}:{key}\n"));
    }
    std::fs::write(path, text).expect("write the key file");
}

/// A running relay with a key file that it rereads.
struct KeyedRelay {
    addr: SocketAddr,
    control: AdmissionControl,
    _signal: StopSignal,
}

async fn start_relay_with_keys(keys: &Path, capacity: Option<usize>) -> KeyedRelay {
    let policy = AdmissionPolicy::Allowlist(load_keys(keys).expect("the key file parses"));
    let control = AdmissionControl::new(policy, ControlConfig::public_default());
    let rendezvous: Arc<Rendezvous<Leg<TcpStream>>> = Arc::new(match capacity {
        Some(capacity) => Rendezvous::with_capacity(capacity),
        None => Rendezvous::new(),
    });
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind the relay");
    let addr = listener.local_addr().expect("relay address");
    let (signal, stop) = StopSignal::new();
    tokio::spawn(watch_keys(
        keys.to_path_buf(),
        control.clone(),
        POLL,
        stop.clone(),
    ));
    tokio::spawn(serve(
        listener,
        control.clone(),
        rendezvous,
        ServeConfig::default(),
        stop,
    ));
    KeyedRelay {
        addr,
        control,
        _signal: signal,
    }
}

/// Dials the relay with `key` on the ticket `ticket` and returns the
/// connection with the relay's answer.
async fn dial(relay: SocketAddr, key: Option<&str>, ticket: u8) -> (TcpStream, RelayAnswer) {
    let mut stream = TcpStream::connect(relay).await.expect("dial the relay");
    let preamble = Preamble::new(
        key.map(AdmissionToken::from_text),
        RendezvousTicket::from_bytes([ticket; RENDEZVOUS_TICKET_LEN]),
    );
    write_preamble(&mut stream, &preamble)
        .await
        .expect("write the preamble");
    let answer = timeout(STEP_TIMEOUT, read_answer(&mut stream))
        .await
        .expect("the relay answers promptly")
        .expect("a well-formed answer");
    (stream, answer)
}

/// Sends one frame from `from` and reads it on `to`: the relay forwards whole
/// frames, a length prefix and an opaque body.
async fn forward_one_frame(from: &mut TcpStream, to: &mut TcpStream, body: &[u8]) {
    let mut frame = (body.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(body);
    from.write_all(&frame).await.expect("write a frame");
    let mut received = vec![0u8; frame.len()];
    timeout(STEP_TIMEOUT, to.read_exact(&mut received))
        .await
        .expect("the frame is forwarded promptly")
        .expect("read the frame");
    assert_eq!(received, frame, "the frame arrives unchanged");
}

/// Dials with `key` again and again until the relay gives `expected`, or fails
/// when it never does. A changed key file takes effect within a few polls.
async fn dial_until(relay: SocketAddr, key: &str, ticket: u8, expected: RelayAnswer) -> TcpStream {
    let attempts = async {
        loop {
            let (stream, answer) = dial(relay, Some(key), ticket).await;
            if answer == expected {
                return stream;
            }
            drop(stream);
            tokio::time::sleep(POLL).await;
        }
    };
    timeout(STEP_TIMEOUT, attempts)
        .await
        .unwrap_or_else(|_| panic!("the relay never answered {expected:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withdrawn_key_is_refused_while_a_session_in_progress_continues() {
    let dir = TempDir::new("withdraw");
    let keys = dir.file("relay.keys");
    write_keys(&keys, &[("acme", KEY_ACME), ("frontdesk", KEY_FRONTDESK)]);
    let relay = start_relay_with_keys(&keys, None).await;

    // Each key is admitted, and a key the file does not list is not.
    let (mut technician, answer) = dial(relay.addr, Some(KEY_ACME), 0x01).await;
    assert_eq!(answer, RelayAnswer::Admitted);
    let (mut customer, answer) = dial(relay.addr, Some(KEY_FRONTDESK), 0x01).await;
    assert_eq!(answer, RelayAnswer::Admitted);
    let (_refused, answer) = dial(relay.addr, Some(KEY_NEWCOMER), 0x02).await;
    assert_eq!(
        answer,
        RelayAnswer::Refused(RelayRefusal::CredentialRejected)
    );
    let (_refused, answer) = dial(relay.addr, None, 0x02).await;
    assert_eq!(
        answer,
        RelayAnswer::Refused(RelayRefusal::CredentialRequired)
    );

    // The two admitted legs share a ticket: a session runs between them.
    forward_one_frame(&mut technician, &mut customer, b"before the change").await;
    forward_one_frame(&mut customer, &mut technician, b"and back").await;

    // The operator withdraws one key and adds another, in the running relay.
    write_keys(&keys, &[("acme", KEY_ACME), ("newcomer", KEY_NEWCOMER)]);
    let withdrawn = RelayAnswer::Refused(RelayRefusal::CredentialRejected);
    dial_until(relay.addr, KEY_FRONTDESK, 0x03, withdrawn).await;
    let _newcomer = dial_until(relay.addr, KEY_NEWCOMER, 0x04, RelayAnswer::Admitted).await;
    let (_acme, answer) = dial(relay.addr, Some(KEY_ACME), 0x05).await;
    assert_eq!(
        answer,
        RelayAnswer::Admitted,
        "an untouched key stays valid"
    );

    // The session of the withdrawn key was admitted before: it continues.
    forward_one_frame(&mut technician, &mut customer, b"after the change").await;
    forward_one_frame(&mut customer, &mut technician, b"still there").await;

    // Events and metrics name the labels and never a key (REQ-S-017).
    let events = relay.control.recent_events();
    for label in ["acme", "frontdesk", "newcomer"] {
        assert!(
            events
                .iter()
                .any(|event| event.kind == OpEventKind::Admitted
                    && event.client == Some(ClientId::new(label))),
            "an admission of {label} is recorded"
        );
    }
    let shown = format!("{events:?} {:?}", relay.control.metrics());
    for key in [KEY_ACME, KEY_FRONTDESK, KEY_NEWCOMER] {
        assert!(!shown.contains(key), "a key appears in {shown}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_file_that_breaks_a_rule_keeps_the_keys_in_force() {
    let dir = TempDir::new("broken");
    let keys = dir.file("relay.keys");
    write_keys(&keys, &[("acme", KEY_ACME)]);
    let relay = start_relay_with_keys(&keys, None).await;

    // The same label twice: the whole file is refused.
    write_keys(&keys, &[("acme", KEY_FRONTDESK), ("acme", KEY_NEWCOMER)]);
    tokio::time::sleep(POLL * 8).await;
    let (_still, answer) = dial(relay.addr, Some(KEY_ACME), 0x11).await;
    assert_eq!(answer, RelayAnswer::Admitted, "the keys from before stay");
    let (_not_yet, answer) = dial(relay.addr, Some(KEY_FRONTDESK), 0x12).await;
    assert_eq!(
        answer,
        RelayAnswer::Refused(RelayRefusal::CredentialRejected),
        "no key of the refused file was taken over"
    );

    // A file that disappears changes nothing either.
    std::fs::remove_file(&keys).expect("remove the key file");
    tokio::time::sleep(POLL * 8).await;
    let (_still, answer) = dial(relay.addr, Some(KEY_ACME), 0x13).await;
    assert_eq!(answer, RelayAnswer::Admitted);

    // The operator repairs the file: now it applies.
    write_keys(&keys, &[("frontdesk", KEY_FRONTDESK)]);
    let withdrawn = RelayAnswer::Refused(RelayRefusal::CredentialRejected);
    dial_until(relay.addr, KEY_ACME, 0x14, withdrawn).await;
    let (_frontdesk, answer) = dial(relay.addr, Some(KEY_FRONTDESK), 0x15).await;
    assert_eq!(answer, RelayAnswer::Admitted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_without_a_place_to_park_says_so() {
    let dir = TempDir::new("full");
    let keys = dir.file("relay.keys");
    write_keys(&keys, &[("acme", KEY_ACME), ("frontdesk", KEY_FRONTDESK)]);
    // One waiting leg fills the table.
    let relay = start_relay_with_keys(&keys, Some(1)).await;

    let (mut waiting, answer) = dial(relay.addr, Some(KEY_ACME), 0x21).await;
    assert_eq!(answer, RelayAnswer::Admitted);

    // Another rendezvous finds no place: refused with the reason, not admitted
    // and then dropped.
    let (mut refused, answer) = dial(relay.addr, Some(KEY_FRONTDESK), 0x22).await;
    assert_eq!(answer, RelayAnswer::Refused(RelayRefusal::Busy));
    let mut rest = [0u8; 1];
    let closed = timeout(STEP_TIMEOUT, refused.read(&mut rest))
        .await
        .expect("the refused leg is closed promptly");
    assert!(matches!(closed, Ok(0) | Err(_)), "nothing is forwarded");

    // The peer of the waiting leg needs no place: it is admitted and the two
    // are forwarded to each other.
    let (mut peer, answer) = dial(relay.addr, Some(KEY_FRONTDESK), 0x21).await;
    assert_eq!(answer, RelayAnswer::Admitted);
    forward_one_frame(&mut peer, &mut waiting, b"matched").await;
}

// ---------------------------------------------------------------------------
// The program itself
// ---------------------------------------------------------------------------

/// Runs `cretlink-relay-server` with `args`, without any relay setting of the
/// environment this test runs in.
fn relay_server(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cretlink-relay-server"));
    for name in [
        "CRETLINK_RELAY_BIND",
        "CRETLINK_RELAY_TOKEN",
        "CRETLINK_RELAY_KEYS",
        "CRETLINK_RELAY_TLS_CERT",
        "CRETLINK_RELAY_TLS_KEY",
    ] {
        command.env_remove(name);
    }
    command.args(args);
    command
}

#[test]
fn keygen_prints_a_new_key_and_nothing_else() {
    let key_of = || {
        let output = relay_server(&["keygen"]).output().expect("run keygen");
        assert!(output.status.success(), "keygen exits in order");
        assert!(output.stderr.is_empty(), "keygen says nothing else");
        String::from_utf8(output.stdout).expect("text")
    };
    let first = key_of();
    let second = key_of();
    assert_eq!(first.trim_end().len(), 64, "256 bits in hexadecimal");
    assert!(first.ends_with('\n'));
    assert!(
        first
            .trim_end()
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_ne!(first, second, "every key is new");

    // A key from keygen is one a key file takes.
    let dir = TempDir::new("keygen");
    let keys = dir.file("relay.keys");
    write_keys(&keys, &[("acme", first.trim_end())]);
    assert_eq!(load_keys(&keys).expect("the file parses").len(), 1);

    let extra = relay_server(&["keygen", "twice"])
        .output()
        .expect("run keygen");
    assert!(!extra.status.success(), "keygen takes no argument");
}

#[test]
fn a_key_file_together_with_a_shared_secret_is_refused_at_startup() {
    let dir = TempDir::new("both");
    let keys = dir.file("relay.keys");
    write_keys(&keys, &[("acme", KEY_ACME)]);
    let path = keys.to_str().expect("a plain path");

    // On the command line, and through the environment in either direction.
    let attempts = [
        relay_server(&["--keys", path, "--token", "a-shared-secret"])
            .output()
            .expect("run"),
        relay_server(&["--keys", path])
            .env("CRETLINK_RELAY_TOKEN", "a-shared-secret")
            .output()
            .expect("run"),
        relay_server(&["--token", "a-shared-secret"])
            .env("CRETLINK_RELAY_KEYS", path)
            .output()
            .expect("run"),
    ];
    for output in attempts {
        assert!(!output.status.success(), "the relay does not start");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.contains("use one of the two"), "{said}");
        assert!(!said.contains("a-shared-secret"), "{said}");
        assert!(!said.contains(KEY_ACME), "{said}");
    }
}

#[test]
fn a_key_file_that_breaks_a_rule_is_refused_at_startup_with_its_line() {
    let dir = TempDir::new("startup");

    // A label listed twice, a key listed twice, a line without a key, no key
    // at all, and a file that is not there.
    let duplicate_label = dir.file("duplicate-label.keys");
    write_keys(
        &duplicate_label,
        &[("acme", KEY_ACME), ("acme", KEY_FRONTDESK)],
    );
    let duplicate_key = dir.file("duplicate-key.keys");
    write_keys(
        &duplicate_key,
        &[("acme", KEY_ACME), ("frontdesk", KEY_ACME)],
    );
    let malformed = dir.file("malformed.keys");
    std::fs::write(&malformed, format!("acme:{KEY_ACME}\nfrontdesk\n")).expect("write");
    let empty = dir.file("empty.keys");
    std::fs::write(&empty, "# nobody yet\n").expect("write");
    let absent = dir.file("absent.keys");

    let cases = [
        (
            &duplicate_label,
            "line 3: the label `acme` is already used on line 2",
        ),
        (
            &duplicate_key,
            "line 3: this key is already listed on line 2",
        ),
        (&malformed, "line 2: expected `label:key`"),
        (&empty, "lists no key"),
        (&absent, "could not be read"),
    ];
    for (file, expected) in cases {
        // An address that is free would let a relay start; the refusal must
        // come before the listener, so a taken port is never reached either.
        let output = relay_server(&["--bind", "127.0.0.1:0", "--keys", file.to_str().unwrap()])
            .output()
            .expect("run");
        assert!(
            !output.status.success(),
            "{expected}: the relay does not start"
        );
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.contains(expected), "expected {expected:?} in {said:?}");
        for key in [KEY_ACME, KEY_FRONTDESK] {
            assert!(!said.contains(key), "a key appears in {said:?}");
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            !printed.contains("listening"),
            "the listener never opened: {printed:?}"
        );
    }
}
