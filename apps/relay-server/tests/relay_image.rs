//! The relay server as a container image (ST-M007-015, REQ-N-023).
//!
//! These tests run against a **built image** and need Docker, so they are
//! ignored in an ordinary test run. Continuous verification builds the image
//! and runs them:
//!
//! ```sh
//! docker build -f apps/relay-server/Dockerfile -t cretlink-relay-server:test .
//! CRETLINK_RELAY_IMAGE=cretlink-relay-server:test \
//!     cargo test -p cretlink-relay-server --test relay_image -- --ignored
//! ```
//!
//! They check what the image promises: one program on an empty base, a fixed
//! user without rights, a root file system that may be read-only, configuration
//! through environment variables and mounted files, a rendezvous that completes
//! through it, and a stop in order when the container is stopped.

#![cfg(unix)]
// `unwrap`/`expect`/`panic` are the assertion vocabulary of the test suites.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use cretlink_relay::{AdmissionToken, RENDEZVOUS_TICKET_LEN, RelayRefusal, RendezvousTicket};
use cretlink_session::{Dial, RelayFailure, SessionError, WssRelayDial};
use cretlink_wss::testcert::self_signed;
use cretlink_wss::{CertificateDer, PrivateKeyDer, WssConnector};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// The largest image the story allows, in bytes.
const MAX_IMAGE_BYTES: u64 = 15 * 1024 * 1024;

/// The user the image runs as.
const IMAGE_USER: &str = "65532:65532";

/// The name in the relay's test certificate.
const RELAY_DNS: &str = "relay.test";

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

const KEY_TECHNICIAN: &str = "technician-0123456789abcdef0123456789";
const KEY_CUSTOMER: &str = "customer-fedcba9876543210fedcba98765";

/// The image under test, from the environment.
fn image() -> String {
    std::env::var("CRETLINK_RELAY_IMAGE")
        .ok()
        .filter(|name| !name.is_empty())
        .expect("set CRETLINK_RELAY_IMAGE to the name of a built relay server image")
}

/// Runs `docker` with `args` and returns what it printed and how it ended.
fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .expect("docker can be run")
}

/// Runs `docker` with `args`, which must succeed, and returns its output.
fn docker_ok(args: &[&str]) -> String {
    let output = docker(args);
    assert!(
        output.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("docker prints text")
        .trim()
        .to_string()
}

/// A container that is removed when the value is dropped, whatever the test
/// did.
struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "--force", &self.0]);
    }
}

/// The options that take every right away from a container: a read-only root
/// file system, no capability, and no way to gain one.
const LOCKED_DOWN: [&str; 5] = [
    "--read-only",
    "--cap-drop",
    "ALL",
    "--security-opt",
    "no-new-privileges",
];

/// A directory of its own for one test, removed when the value is dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cretlink-relay-image-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the test directory");
        // The container's user is not the owner: it needs to enter the folder.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("open the test directory");
        Self(path)
    }

    /// Writes a file that every user may read, as a mounted secret is to the
    /// user of the container.
    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, content).expect("write a test file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))
            .expect("make the test file readable");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Encodes DER bytes as a PEM block.
fn pem(label: &str, der: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in der.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                encoded.push(ALPHABET[((bits >> (18 - 6 * index)) & 0x3f) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    let mut text = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        text.push('\n');
    }
    text.push_str(&format!("-----END {label}-----\n"));
    text
}

/// The names and sizes of the regular files in a tar archive.
fn files_of_tar(archive: &[u8]) -> Vec<(String, u64)> {
    let mut files = Vec::new();
    let mut offset = 0;
    while offset + 512 <= archive.len() {
        let header = &archive[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let text = |range: std::ops::Range<usize>| {
            String::from_utf8_lossy(&header[range])
                .trim_end_matches('\0')
                .trim()
                .to_string()
        };
        let name = text(0..100);
        let size = u64::from_str_radix(&text(124..136), 8).expect("an octal size");
        // '0' and NUL are regular files.
        if matches!(header[156], b'0' | 0) {
            files.push((name, size));
        }
        offset += 512 + (size as usize).div_ceil(512) * 512;
    }
    files
}

#[test]
#[ignore = "needs a built image: set CRETLINK_RELAY_IMAGE and run with --ignored"]
fn the_image_holds_one_program_on_an_empty_base() {
    let image = image();

    // Small, and with the fixed user that has no rights.
    let size: u64 = docker_ok(&["image", "inspect", &image, "--format", "{{.Size}}"])
        .parse()
        .expect("a size in bytes");
    assert!(
        size < MAX_IMAGE_BYTES,
        "the image is {size} bytes, more than {MAX_IMAGE_BYTES}"
    );
    assert_eq!(
        docker_ok(&["image", "inspect", &image, "--format", "{{.Config.User}}"]),
        IMAGE_USER
    );
    assert_eq!(
        docker_ok(&[
            "image",
            "inspect",
            &image,
            "--format",
            "{{json .Config.Entrypoint}}"
        ]),
        r#"["/cretlink-relay-server"]"#
    );

    // Nothing in it but the program, its licence and the notices. A container
    // runtime adds empty files of its own (such as /etc/hosts); every file
    // with content must be one of the three.
    let container = Container(docker_ok(&["create", &image]));
    let export = docker(&["export", &container.0]);
    assert!(export.status.success(), "the file system can be exported");
    let mut with_content: Vec<String> = files_of_tar(&export.stdout)
        .into_iter()
        .filter(|(_name, size)| *size > 0)
        .map(|(name, _size)| name)
        .collect();
    with_content.sort();
    assert_eq!(
        with_content,
        [
            "LICENSE",
            "THIRD-PARTY-NOTICES.txt",
            "cretlink-relay-server"
        ]
    );

    // No shell to start.
    for shell in ["/bin/sh", "/bin/bash", "sh"] {
        let run = docker(&["run", "--rm", "--entrypoint", shell, &image, "-c", "true"]);
        assert!(!run.status.success(), "{shell} must not exist in the image");
    }
}

#[test]
#[ignore = "needs a built image: set CRETLINK_RELAY_IMAGE and run with --ignored"]
fn the_program_runs_without_any_right() {
    let image = image();
    let mut args = vec!["run", "--rm"];
    args.extend(LOCKED_DOWN);
    args.extend([image.as_str(), "keygen"]);
    let key = docker_ok(&args);
    assert_eq!(key.len(), 64, "keygen prints a key: {key:?}");
    assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

/// A connector that trusts exactly the relay's test certificate.
fn pinned_connector(cert: CertificateDer<'static>) -> WssConnector {
    let roots = cretlink_wss::pinned_roots(vec![cert]).expect("pin the relay leaf");
    let config = cretlink_wss::client_config(roots).expect("build client config");
    cretlink_wss::connector(config)
}

/// Starts the image with a certificate, a private key and a key file mounted
/// read-only, and returns the container with the address it listens on.
fn start_relay(image: &str, config: &Path) -> (Container, SocketAddr) {
    let mount = format!("{}:/config:ro", config.display());
    let mut args = vec!["run", "--detach"];
    args.extend(LOCKED_DOWN);
    args.extend([
        "--publish",
        "127.0.0.1::9970",
        "--volume",
        &mount,
        "--env",
        "CRETLINK_RELAY_TLS_CERT=/config/relay.crt",
        "--env",
        "CRETLINK_RELAY_TLS_KEY=/config/relay.key",
        "--env",
        "CRETLINK_RELAY_KEYS=/config/relay.keys",
        image,
    ]);
    let container = Container(docker_ok(&args));
    let published = docker_ok(&["port", &container.0, "9970/tcp"]);
    let addr = published
        .lines()
        .next()
        .expect("a published port")
        .parse()
        .expect("an address");
    (container, addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a built image: set CRETLINK_RELAY_IMAGE and run with --ignored"]
async fn a_rendezvous_completes_through_the_image() {
    let image = image();

    // What an operator mounts: a certificate, its key and the key file.
    let (cert, key) = self_signed(RELAY_DNS).expect("mint the relay's certificate");
    let PrivateKeyDer::Pkcs8(key_der) = &key else {
        panic!("the test certificate has a PKCS#8 key");
    };
    let config = TempDir::new("config");
    config.write("relay.crt", &pem("CERTIFICATE", cert.as_ref()));
    config.write("relay.key", &pem("PRIVATE KEY", key_der.secret_pkcs8_der()));
    config.write(
        "relay.keys",
        &format!("sam-laptop:{KEY_TECHNICIAN}\nacme:{KEY_CUSTOMER}\n"),
    );
    let (container, relay) = start_relay(&image, &config.0);

    // The relay is up when its port answers.
    let started = Instant::now();
    while TcpStream::connect(relay).await.is_err() {
        assert!(
            started.elapsed() < STEP_TIMEOUT,
            "the relay did not start: {}",
            docker_ok(&["logs", &container.0])
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Two endpoints, each with its own key, meet on one ticket over wss.
    let connector = pinned_connector(cert);
    let ticket = || RendezvousTicket::from_bytes([0x91; RENDEZVOUS_TICKET_LEN]);
    let dial = |key: &str| {
        WssRelayDial::new(
            relay,
            RELAY_DNS,
            connector.clone(),
            ticket(),
            Some(AdmissionToken::from_text(key)),
        )
    };
    let mut technician = timeout(STEP_TIMEOUT, dial(KEY_TECHNICIAN).dial())
        .await
        .expect("the dial resolves")
        .expect("the technician's key is admitted");
    let mut customer = timeout(STEP_TIMEOUT, dial(KEY_CUSTOMER).dial())
        .await
        .expect("the dial resolves")
        .expect("the customer's key is admitted");

    // The relay forwards whole frames in both directions.
    for (body, to_customer) in [
        (b"through the image".as_slice(), true),
        (b"and back".as_slice(), false),
    ] {
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(body);
        let (from, to) = if to_customer {
            (&mut technician, &mut customer)
        } else {
            (&mut customer, &mut technician)
        };
        from.write_all(&frame).await.expect("write a frame");
        from.flush().await.expect("flush");
        let mut received = vec![0u8; frame.len()];
        timeout(STEP_TIMEOUT, to.read_exact(&mut received))
            .await
            .expect("the frame is forwarded")
            .expect("read the frame");
        assert_eq!(received, frame);
    }

    // A key that the file does not list is refused with the reason.
    let refused = timeout(STEP_TIMEOUT, dial("a-key-the-file-does-not-list").dial())
        .await
        .expect("the dial resolves")
        .expect_err("an unknown key is refused");
    assert!(
        matches!(
            refused,
            SessionError::Relay(RelayFailure::Refused(RelayRefusal::CredentialRejected))
        ),
        "{refused:?}"
    );

    // Stopping the container stops the relay in order: it handles the
    // termination signal, so it ends at once and with success.
    drop((technician, customer));
    let stopping = Instant::now();
    docker_ok(&["stop", "--time", "20", &container.0]);
    assert!(
        stopping.elapsed() < Duration::from_secs(10),
        "the relay did not stop on the termination signal"
    );
    assert_eq!(
        docker_ok(&["inspect", &container.0, "--format", "{{.State.ExitCode}}"]),
        "0"
    );

    // Its log names the labels and never a key.
    let logs = docker(&["logs", &container.0]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&logs.stdout),
        String::from_utf8_lossy(&logs.stderr)
    );
    assert!(said.contains("wss://"), "{said}");
    assert!(said.contains("one key per label (2 keys"), "{said}");
    assert!(said.contains("sam-laptop"), "{said}");
    assert!(said.contains("acme"), "{said}");
    assert!(said.contains("Relay metrics at shutdown"), "{said}");
    for key in [KEY_TECHNICIAN, KEY_CUSTOMER] {
        assert!(!said.contains(key), "a key appears in the log: {said}");
    }
}
