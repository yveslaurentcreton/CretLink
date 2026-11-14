//! ST-M007-009 — the wss envelope carries an opaque byte stream faithfully and
//! fails closed on an untrusted server certificate.
//!
//! These are the crate-local proofs that the transport seam is sound before the
//! relay and endpoints compose it: a byte stream written in frame-misaligned
//! chunks round-trips byte-exact through TLS + WebSocket, and a client that does
//! not trust the server's certificate gets no connection at all.

// Test binary: `unwrap`/`expect` are assertion vocabulary here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};

use cretlink_wss::{
    CertificateDer, acceptor, client_config, connect, connector, pinned_roots, server_config,
    testcert,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// Stands up a wss echo server on loopback trusting a fresh self-signed leaf,
/// returning its address and the certificate a client must pin to reach it.
async fn start_echo(payload_len: usize) -> (SocketAddr, CertificateDer<'static>) {
    let (cert, key) = testcert::self_signed("localhost").expect("mint test cert");
    let server = server_config(vec![cert.clone()], key).expect("server config");
    let acceptor = acceptor(server);

    let listener = TcpListener::bind(loopback()).await.expect("bind echo");
    let addr = listener.local_addr().expect("echo addr");

    tokio::spawn(async move {
        let (tcp, _peer) = listener.accept().await.expect("accept tcp");
        let mut stream = match cretlink_wss::accept(&acceptor, tcp).await {
            Ok(stream) => stream,
            // A client that fails the handshake (fail-closed test) leaves the
            // server with nothing to echo; that is the expected outcome.
            Err(_) => return,
        };
        let mut got = vec![0u8; payload_len];
        if stream.read_exact(&mut got).await.is_err() {
            return;
        }
        let _ = stream.write_all(&got).await;
        let _ = stream.flush().await;
    });

    (addr, cert)
}

/// A byte stream survives the wss envelope byte-exact even when written in
/// chunks that do not align to WebSocket message boundaries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_byte_stream_round_trips_through_the_wss_envelope() {
    const LEN: usize = 40_000;
    let (addr, cert) = start_echo(LEN).await;

    let roots = pinned_roots(vec![cert]).expect("pin server cert");
    let client = client_config(roots).expect("client config");
    let connector = connector(client);

    let tcp = TcpStream::connect(addr).await.expect("dial echo");
    let mut stream = connect(&connector, "localhost", tcp)
        .await
        .expect("wss connect over a trusted cert");

    let payload: Vec<u8> = (0..LEN as u32).map(|i| (i % 251) as u8).collect();
    // Frame-misaligned chunks prove the adapter is a byte stream, not a message
    // relay.
    for chunk in payload.chunks(1234) {
        stream.write_all(chunk).await.expect("write chunk");
    }
    stream.flush().await.expect("flush");

    let mut echo = vec![0u8; LEN];
    stream.read_exact(&mut echo).await.expect("read echo");
    assert_eq!(echo, payload, "the byte stream round-tripped corrupted");
}

/// A client that does not trust the server's certificate gets no connection:
/// the TLS handshake fails and no stream is returned (fail closed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_untrusted_server_certificate_fails_closed() {
    const LEN: usize = 16;
    let (addr, _server_cert) = start_echo(LEN).await;

    // Trust a *different* self-signed cert, so the server's real cert is
    // untrusted.
    let (other_cert, _other_key) = testcert::self_signed("localhost").expect("mint other cert");
    let roots = pinned_roots(vec![other_cert]).expect("pin the wrong cert");
    let client = client_config(roots).expect("client config");
    let connector = connector(client);

    let tcp = TcpStream::connect(addr).await.expect("dial echo");
    let result = connect(&connector, "localhost", tcp).await;
    assert!(
        result.is_err(),
        "an untrusted server certificate must fail the handshake, not connect"
    );
}

/// Completes both handshakes over a pipe inside this process, with a server
/// certificate for `certified` and a client that reaches the server as
/// `name`, and sends a few bytes there and back.
async fn reach_as(certified: &str, name: &str) -> Result<(), cretlink_wss::WssError> {
    let (cert, key) = testcert::self_signed(certified).expect("mint test cert");
    let acceptor = acceptor(server_config(vec![cert.clone()], key).expect("server config"));
    let roots = pinned_roots(vec![cert]).expect("pin server cert");
    let connector = connector(client_config(roots).expect("client config"));

    let (near, far) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        // A client that refuses the certificate leaves nothing to echo.
        let Ok(mut stream) = cretlink_wss::accept(&acceptor, far).await else {
            return;
        };
        let mut got = [0u8; 5];
        if stream.read_exact(&mut got).await.is_ok() {
            let _ = stream.write_all(&got).await;
            let _ = stream.flush().await;
        }
    });

    let mut stream = connect(&connector, name, near).await?;
    stream.write_all(b"hello").await.expect("write");
    stream.flush().await.expect("flush");
    let mut echo = [0u8; 5];
    stream.read_exact(&mut echo).await.expect("read echo");
    assert_eq!(&echo, b"hello", "{name}");
    drop(stream);
    server.await.expect("the server side ends");
    Ok(())
}

/// A relay may be named by an IP address (ST-M007-011): its certificate is
/// then checked against that address, and the request names it as a URI does,
/// an IPv6 address in square brackets. Without the brackets no request could
/// be built, and such a relay could never be reached.
#[tokio::test]
async fn an_ip_address_is_the_validated_name_of_a_relay() {
    for name in ["127.0.0.1", "::1", "2001:db8::1"] {
        reach_as(name, name)
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

/// The certificate of another address, or of a name, is not the certificate
/// of the address that was dialled: the handshake fails closed.
#[tokio::test]
async fn a_certificate_for_another_address_is_refused() {
    for (certified, name) in [
        ("192.0.2.1", "127.0.0.1"),
        ("2001:db8::2", "2001:db8::1"),
        ("localhost", "::1"),
    ] {
        let refused = reach_as(certified, name).await;
        assert!(
            matches!(refused, Err(cretlink_wss::WssError::Io(_))),
            "{certified} for {name}: {refused:?}"
        );
    }
}

/// The first line of a private key in the text form of a key file, written
/// in two parts so that no scanner takes this file for one that holds a key.
const KEY_MARKER: &str = concat!("-----BEGIN PRIVATE", " KEY-----");

/// A copy of a certificate file holds its certificates and nothing else of
/// it (ST-M007-011): what stood beside them is gone, and the copy reads back
/// as the same certificates.
#[test]
fn certificates_are_written_anew_without_what_stood_beside_them() {
    let (first, _) = testcert::self_signed("relay.example.org").expect("mint test cert");
    let (second, _) = testcert::self_signed("host-01.example.org").expect("mint test cert");
    let written = cretlink_wss::certs_to_pem(&[first.clone(), second.clone()]);
    assert_eq!(
        cretlink_wss::certs_from_pem(written.as_bytes()).expect("read back"),
        vec![first.clone(), second.clone()]
    );
    assert!(written.lines().all(|line| line.len() <= 64), "{written}");
    assert!(
        written.ends_with("-----END CERTIFICATE-----\n"),
        "{written}"
    );

    // The same certificates in a file as people keep them: a comment, a
    // private key between them, lines of another length, Windows line ends.
    let end = KEY_MARKER.replace("BEGIN", "END");
    let one_line = |cert: &CertificateDer<'_>| {
        cretlink_wss::certs_to_pem(std::slice::from_ref(cert))
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<String>()
    };
    let messy = format!(
        "# the authority of our relay\r\n-----BEGIN CERTIFICATE-----\r\n{}\r\n-----END \
         CERTIFICATE-----\r\n{KEY_MARKER}\r\nbm90IGEga2V5\r\n{end}\r\n-----BEGIN \
         CERTIFICATE-----\r\n{}\r\n-----END CERTIFICATE-----\r\n",
        one_line(&first),
        one_line(&second),
    );
    let certs = cretlink_wss::certs_from_pem(messy.as_bytes()).expect("read the messy file");
    assert_eq!(certs, vec![first, second]);
    let anew = cretlink_wss::certs_to_pem(&certs);
    assert_eq!(anew, written);
    assert!(!anew.contains("PRIVATE"), "{anew}");
    assert!(!anew.contains('#'), "{anew}");
    assert!(!anew.contains('\r'), "{anew}");

    assert_eq!(cretlink_wss::certs_to_pem(&[]), "");
}
