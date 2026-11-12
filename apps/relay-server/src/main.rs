//! CretLink internet relay server entry point (ST-M007-007, ADR-0016 §5).
//!
//! This binary is the runnable face of the portable [`cretlink_relay`] core: it
//! binds a real TCP listener, builds the admission gate from the documented
//! public posture ([`ControlConfig::public_default`]), and hands both to
//! [`cretlink_relay_server::serve`], which forwards opaque end-to-end Noise
//! sessions between outbound-dialing endpoints. The relay is never a command
//! authority or a plaintext endpoint (REQ-F-022, REQ-S-005); it holds no session
//! key and decodes no protocol message.
//!
//! # Configuration
//!
//! - `--bind <addr>` or `CRETLINK_RELAY_BIND` — the listen address
//!   (default [`DEFAULT_BIND`]). A command-line flag overrides the environment.
//! - `--token <secret>` or `CRETLINK_RELAY_TOKEN` — when set, admission requires
//!   this shared secret ([`AdmissionPolicy::SharedToken`]); when unset the relay
//!   is [`Open`](AdmissionPolicy::Open) and admits every peer under its own label
//!   (rate limits and quotas still apply). The secret is **never** echoed — pass
//!   it through the environment in production so it does not appear in a process
//!   listing.
//! - `--client-label <label>` — the non-secret accountability label all holders
//!   of the shared secret are logged under (default [`DEFAULT_CLIENT_LABEL`]);
//!   ignored under the open policy.
//! - `--keys <file>` or `CRETLINK_RELAY_KEYS` — one admission key per customer
//!   copy and technician, one `label:key` per line (ST-M007-014, see
//!   [`cretlink_relay_server::keys`]). It replaces `--token`: giving both is
//!   refused. A file that breaks a rule is refused at startup with the number
//!   of the line. A change of the file takes effect within seconds, without a
//!   restart and without touching the sessions in progress.
//! - `keygen` — prints a new random key for such a file and exits.
//! - `--version` — prints the release version and exits.
//! - `--behind-proxy` or `CRETLINK_RELAY_BEHIND_PROXY=1` — serve WebSocket
//!   **without** TLS, for a reverse proxy that ends TLS in front of the relay
//!   (ST-M007-015). The endpoints still speak `wss` to the proxy. It excludes
//!   `--tls-cert`/`--tls-key`. The relay does not encrypt the hop from the
//!   proxy, so that hop must be the same host or a private network.
//! - `--tls-cert <path>` / `--tls-key <path>` (or `CRETLINK_RELAY_TLS_CERT` /
//!   `CRETLINK_RELAY_TLS_KEY`) — the operator's PEM certificate chain and private
//!   key. When **both** are supplied the relay serves `wss://` (WebSocket over
//!   TLS, the ADR-0016 §3 shipped default, ST-M007-009) and authenticates itself
//!   to each endpoint; when **neither** is supplied it serves raw TCP (the
//!   ADR-0016 §5 documented interim). Supplying only one, or a missing/unreadable/
//!   malformed cert or key, is refused **at startup** — the relay never falls back
//!   to an unprotected listener (REQ-S-019, REQ-S-020, REQ-N-015). Certificate
//!   rotation is a drain-and-restart with the new material, exactly like the
//!   existing stateless upgrade flow.
//!
//! On `Ctrl+C` the relay stops accepting, tears down any in-flight forward
//! fail-closed (REQ-N-008), and prints a final metadata-only metrics snapshot.
//!

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use cretlink_relay::{
    AdmissionControl, AdmissionPolicy, AdmissionToken, ClientId, ControlConfig, MetricsSnapshot,
    Rendezvous, StopSignal, generate_key,
};
use cretlink_relay_server::keys::{DEFAULT_KEYS_POLL, load_keys, watch_keys};
use cretlink_relay_server::{Leg, ServeConfig, serve, serve_ws, serve_wss};
use cretlink_wss::{WsByteStream, WssAcceptor, WssServerStream};
use tokio::net::{TcpListener, TcpStream};

/// The default listen address: every interface, on the CretLink relay port. A
/// relay is meant to be publicly reachable (REQ-S-010), so it binds all
/// interfaces by default; an operator narrows it with `--bind`.
const DEFAULT_BIND: &str = "0.0.0.0:9970";

/// The default accountability label for the shared-token policy.
const DEFAULT_CLIENT_LABEL: &str = "shared";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The relay's configuration, resolved from flags and the environment.
struct Config {
    bind: String,
    token: Option<AdmissionToken>,
    client_label: String,
    /// Path to the operator's PEM certificate chain, if serving `wss://`.
    tls_cert: Option<String>,
    /// Path to the operator's PEM private key, if serving `wss://`.
    tls_key: Option<String>,
    /// Path to the file with one admission key per label (ST-M007-014).
    keys: Option<String>,
    /// Serve WebSocket without TLS, behind a proxy that ends TLS (ST-M007-015).
    behind_proxy: bool,
}

/// What the command line asks for.
enum Invocation {
    /// `--help` or `--version` was handled and printed.
    Help,
    /// Print a new admission key and exit.
    Keygen,
    /// Run the relay.
    Serve(Config),
}

/// The resolved transport: raw TCP (ADR-0016 §5 interim) or `wss://` (ADR-0016 §3
/// shipped default) with the operator's TLS acceptor already built.
enum Transport {
    /// Serve raw TCP — no `--tls-cert`/`--tls-key` supplied.
    RawTcp,
    /// Serve WebSocket-over-TLS with the operator's certificate.
    Wss(WssAcceptor),
    /// Serve WebSocket without TLS, behind a reverse proxy that ends TLS.
    BehindProxy,
}

/// Resolves the transport from the cert/key paths, **failing closed at startup**:
/// both paths present builds and returns the wss acceptor (reading and parsing the
/// PEM here, so a malformed cert/key is refused before the listener opens); exactly
/// one present is a configuration error; neither is the documented raw-TCP interim.
/// No branch ever falls back to an unprotected listener (REQ-S-019, REQ-S-020,
/// REQ-N-015).
fn resolve_transport(config: &Config) -> Result<Transport, String> {
    // Behind a proxy the proxy holds the certificate. Asking for both is a
    // mistake that is refused, never resolved in favour of one of them.
    if config.behind_proxy {
        return if config.tls_cert.is_some() || config.tls_key.is_some() {
            Err(
                "--behind-proxy serves WebSocket without TLS, for a reverse proxy that holds \
                 the certificate; it cannot be combined with --tls-cert or --tls-key (or \
                 CRETLINK_RELAY_TLS_CERT and CRETLINK_RELAY_TLS_KEY)"
                    .to_string(),
            )
        } else {
            Ok(Transport::BehindProxy)
        };
    }
    match (&config.tls_cert, &config.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert_pem = std::fs::read(cert_path).map_err(|err| {
                format!("could not read the TLS certificate from {cert_path}: {err}")
            })?;
            let key_pem = std::fs::read(key_path).map_err(|err| {
                format!("could not read the TLS private key from {key_path}: {err}")
            })?;
            let certs = cretlink_wss::certs_from_pem(&cert_pem)
                .map_err(|err| format!("the TLS certificate in {cert_path} is invalid: {err}"))?;
            let key = cretlink_wss::private_key_from_pem(&key_pem)
                .map_err(|err| format!("the TLS private key in {key_path} is invalid: {err}"))?;
            let server_config = cretlink_wss::server_config(certs, key).map_err(|err| {
                format!("the TLS certificate and private key were rejected (do they match?): {err}")
            })?;
            Ok(Transport::Wss(cretlink_wss::acceptor(server_config)))
        }
        (Some(_), None) => Err(
            "a TLS certificate was supplied without a private key; pass both --tls-cert and \
             --tls-key (or CRETLINK_RELAY_TLS_CERT and CRETLINK_RELAY_TLS_KEY) to serve wss, or \
             neither to serve raw TCP"
                .to_string(),
        ),
        (None, Some(_)) => Err(
            "a TLS private key was supplied without a certificate; pass both --tls-cert and \
             --tls-key (or CRETLINK_RELAY_TLS_CERT and CRETLINK_RELAY_TLS_KEY) to serve wss, or \
             neither to serve raw TCP"
                .to_string(),
        ),
        (None, None) => Ok(Transport::RawTcp),
    }
}

async fn run() -> Result<(), String> {
    let config = match parse_args()? {
        Invocation::Serve(config) => config,
        // `--help` was handled and printed; exit cleanly.
        Invocation::Help => return Ok(()),
        // The key goes to standard output and nowhere else, so that an
        // operator can redirect it into a file.
        Invocation::Keygen => {
            let key = generate_key().map_err(|err| err.to_string())?;
            println!("{key}");
            return Ok(());
        }
    };

    // Resolve the transport BEFORE binding (and before `config` is consumed): a
    // malformed or half-supplied cert/key is refused here, so the relay never
    // opens a listener it cannot protect (fail-closed startup — REQ-S-019,
    // REQ-S-020, REQ-N-015).
    let transport = resolve_transport(&config)?;
    let transport_label = match &transport {
        Transport::Wss(_) => "wss:// (WebSocket over TLS; the relay authenticates itself)",
        Transport::BehindProxy => {
            "WebSocket without TLS, for a reverse proxy that ends TLS in front of this relay \
             (the relay does not encrypt the connection from the proxy: keep it on this host \
             or a private network; sessions stay end-to-end encrypted regardless)"
        }
        Transport::RawTcp => {
            "raw TCP (ADR-0016 §5 interim — no transport encryption; sessions stay end-to-end \
             encrypted regardless)"
        }
    };

    // The admission posture: one key per label from a file, a shared secret,
    // or an open relay. Either way the rate limiter and concurrency quotas from
    // the public default still bound abuse (ADR-0016 §4–§5). A key file that
    // breaks a rule is refused here, before the listener opens (REQ-N-015).
    let keys_path = config.keys.map(PathBuf::from);
    let policy = match (&keys_path, config.token) {
        (Some(path), None) => {
            AdmissionPolicy::Allowlist(load_keys(path).map_err(|err| err.to_string())?)
        }
        (None, Some(token)) => AdmissionPolicy::SharedToken {
            token,
            client: ClientId::new(config.client_label),
        },
        (None, None) => AdmissionPolicy::Open,
        // `parse_args` refuses the combination; never fall back to one of the
        // two silently.
        (Some(_), Some(_)) => return Err(KEYS_AND_TOKEN.to_string()),
    };
    let policy_label = match &policy {
        AdmissionPolicy::Open => "open (no credential required)".to_string(),
        AdmissionPolicy::SharedToken { .. } => "shared-token".to_string(),
        AdmissionPolicy::Allowlist(keys) => {
            format!(
                "one key per label ({} keys, reread when the file changes)",
                keys.len()
            )
        }
    };

    let control = AdmissionControl::new(policy, ControlConfig::public_default());

    let listener = TcpListener::bind(&config.bind).await.map_err(|err| {
        format!(
            "could not bind the relay listener to {}: {err}",
            config.bind
        )
    })?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("could not read the bound relay address: {err}"))?;

    println!(
        "CretLink relay server {} (ST-M007-007, wss ST-M007-009)",
        cretlink_shared::RELEASE_VERSION
    );
    println!("  listening: {local_addr}");
    println!("  transport: {transport_label}");
    println!("  admission: {policy_label}");
    println!("  forwarding opaque end-to-end Noise sessions only — no session key is held here.");
    println!("  press Ctrl+C to stop.");

    // The stop signal drives a graceful shutdown: the accept loop stops taking
    // new connections and every in-flight splice (each holds a clone) is torn
    // down fail-closed rather than abandoned (REQ-N-008). Each transport builds
    // its own typed rendezvous over the same core `serve`/`serve_wss` pipeline.
    let (signal, stop) = StopSignal::new();
    // A changed key file is applied while the relay runs (ST-M007-014).
    if let Some(path) = keys_path {
        tokio::spawn(watch_keys(
            path,
            control.clone(),
            DEFAULT_KEYS_POLL,
            stop.clone(),
        ));
    }
    let serve_task = match transport {
        Transport::Wss(acceptor) => {
            let rendezvous: Arc<Rendezvous<Leg<WssServerStream<TcpStream>>>> =
                Arc::new(Rendezvous::new());
            tokio::spawn(serve_wss(
                listener,
                acceptor,
                control.clone(),
                rendezvous,
                ServeConfig::default(),
                stop,
            ))
        }
        Transport::BehindProxy => {
            let rendezvous: Arc<Rendezvous<Leg<WsByteStream<TcpStream>>>> =
                Arc::new(Rendezvous::new());
            tokio::spawn(serve_ws(
                listener,
                control.clone(),
                rendezvous,
                ServeConfig::default(),
                stop,
            ))
        }
        Transport::RawTcp => {
            let rendezvous: Arc<Rendezvous<Leg<TcpStream>>> = Arc::new(Rendezvous::new());
            tokio::spawn(serve(
                listener,
                control.clone(),
                rendezvous,
                ServeConfig::default(),
                stop,
            ))
        }
    };

    // Wait for the operator's stop. A failure to install the handler is itself
    // fatal — better to refuse to run than to run un-stoppable.
    stop_requested().await?;

    println!("\nStopping — refusing new connections and tearing down in-flight forwards.");
    signal.stop();
    if let Err(err) = serve_task.await {
        // The accept loop panicking is a bug; surface it rather than hide it.
        return Err(format!(
            "the relay accept loop terminated abnormally: {err}"
        ));
    }

    print_metrics(&control.metrics());
    Ok(())
}

/// Waits for the operator's stop: Ctrl+C, or the termination signal that a
/// service manager or a container runtime sends (ST-M007-015). In a container
/// the relay is the first process, for which a signal without a handler does
/// nothing: without this, stopping the container would end in a kill after a
/// wait, with every forward cut instead of torn down in order.
async fn stop_requested() -> Result<(), String> {
    let interrupted = || async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|err| format!("could not listen for Ctrl+C: {err}"))
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate())
            .map_err(|err| format!("could not listen for the termination signal: {err}"))?;
        tokio::select! {
            result = interrupted() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        interrupted().await
    }
}

/// The refusal of a key file together with a shared secret.
const KEYS_AND_TOKEN: &str = "both a key file and a shared admission secret were given (--keys or \
     CRETLINK_RELAY_KEYS, and --token or CRETLINK_RELAY_TOKEN); use one of the two";

/// Parses the command line and folds in environment defaults, or returns an
/// actionable error string for a malformed invocation.
fn parse_args() -> Result<Invocation, String> {
    let mut bind = std::env::var("CRETLINK_RELAY_BIND").ok();
    let mut token_text = std::env::var("CRETLINK_RELAY_TOKEN").ok();
    let mut client_label: Option<String> = None;
    let mut tls_cert = std::env::var("CRETLINK_RELAY_TLS_CERT").ok();
    let mut tls_key = std::env::var("CRETLINK_RELAY_TLS_KEY").ok();
    let mut keys = std::env::var("CRETLINK_RELAY_KEYS").ok();
    let mut behind_proxy = match std::env::var("CRETLINK_RELAY_BEHIND_PROXY") {
        Ok(value) => parse_switch("CRETLINK_RELAY_BEHIND_PROXY", &value)?,
        Err(_) => false,
    };

    let mut args = std::env::args().skip(1).peekable();
    // `keygen` is the one subcommand; it takes nothing else.
    if args.peek().map(String::as_str) == Some("keygen") {
        args.next();
        return match args.next() {
            None => Ok(Invocation::Keygen),
            Some(extra) => Err(format!("keygen takes no argument, but got '{extra}'")),
        };
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage();
                return Ok(Invocation::Help);
            }
            "--version" | "-V" => {
                println!("cretlink-relay-server {}", cretlink_shared::RELEASE_VERSION);
                return Ok(Invocation::Help);
            }
            "--behind-proxy" => behind_proxy = true,
            "--keys" => {
                keys = Some(args.next().ok_or(
                    "--keys requires the path of a file with one label:key per line, e.g. --keys relay.keys",
                )?);
            }
            "--bind" => {
                bind = Some(
                    args.next()
                        .ok_or("--bind requires an address, e.g. --bind 0.0.0.0:9970")?,
                );
            }
            "--token" => {
                token_text = Some(
                    args.next()
                        .ok_or("--token requires a value (prefer CRETLINK_RELAY_TOKEN so the secret is not visible in a process listing)")?,
                );
            }
            "--client-label" => {
                client_label = Some(
                    args.next()
                        .ok_or("--client-label requires a value, e.g. --client-label acme")?,
                );
            }
            "--tls-cert" => {
                tls_cert = Some(
                    args.next()
                        .ok_or("--tls-cert requires a path to a PEM certificate chain, e.g. --tls-cert relay.crt")?,
                );
            }
            "--tls-key" => {
                tls_key = Some(args.next().ok_or(
                    "--tls-key requires a path to a PEM private key, e.g. --tls-key relay.key",
                )?);
            }
            other => {
                return Err(format!("unknown argument '{other}' (try --help for usage)"));
            }
        }
    }

    // A token given as an empty string is a configuration mistake, not an
    // anonymous open relay — refuse it rather than silently admitting everyone.
    let token = match token_text {
        Some(text) if text.is_empty() => {
            return Err(
                "the admission token is empty; unset CRETLINK_RELAY_TOKEN for an open relay, or provide a non-empty secret".to_string(),
            );
        }
        Some(text) => Some(AdmissionToken::from_text(&text)),
        None => None,
    };

    // An empty path is a configuration mistake, like an empty secret.
    if keys.as_deref() == Some("") {
        return Err(
            "the key file path is empty; unset CRETLINK_RELAY_KEYS or name a file".to_string(),
        );
    }
    if keys.is_some() && token.is_some() {
        return Err(KEYS_AND_TOKEN.to_string());
    }

    Ok(Invocation::Serve(Config {
        bind: bind.unwrap_or_else(|| DEFAULT_BIND.to_string()),
        token,
        client_label: client_label.unwrap_or_else(|| DEFAULT_CLIENT_LABEL.to_string()),
        tls_cert,
        tls_key,
        keys,
        behind_proxy,
    }))
}

/// Reads an on/off environment variable. An unknown value is refused rather
/// than read as off: a relay that was meant to run behind a proxy must not
/// start as something else because of a typing error.
fn parse_switch(name: &str, value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!(
            "{name} is \"{other}\"; use 1 or 0 (or true/false, yes/no, on/off)"
        )),
    }
}

fn print_usage() {
    println!("CretLink internet relay server (ST-M007-007, wss ST-M007-009)\n");
    println!("Usage: cretlink-relay-server [--bind <addr>] [--keys <file> | --token <secret>]");
    println!("                            [--client-label <label>]");
    println!("                            [--tls-cert <path> --tls-key <path> | --behind-proxy]");
    println!("       cretlink-relay-server keygen\n");
    println!("Commands:");
    println!("  keygen                 Print a new random admission key and exit.\n");
    println!("Options:");
    println!(
        "  --bind <addr>          Listen address (env CRETLINK_RELAY_BIND; default {DEFAULT_BIND})."
    );
    println!(
        "  --token <secret>       Require this shared admission secret (env CRETLINK_RELAY_TOKEN)."
    );
    println!(
        "                         Prefer the environment variable so the secret is not visible"
    );
    println!("                         in a process listing. Unset for an open relay.");
    println!(
        "  --client-label <label> Accountability label for shared-token clients (default {DEFAULT_CLIENT_LABEL})."
    );
    println!("  --keys <file>          One admission key per customer copy and technician, one");
    println!("                         label:key per line (env CRETLINK_RELAY_KEYS). Not together");
    println!("                         with --token. A change of the file takes effect within");
    println!("                         seconds; sessions in progress continue.");
    println!(
        "  --tls-cert <path>      PEM certificate chain the relay presents (env CRETLINK_RELAY_TLS_CERT)."
    );
    println!(
        "  --tls-key <path>       PEM private key for that certificate (env CRETLINK_RELAY_TLS_KEY)."
    );
    println!(
        "                         Supply BOTH to serve wss:// (WebSocket over TLS, the shipped"
    );
    println!(
        "                         default transport); supply NEITHER for the raw-TCP interim."
    );
    println!(
        "                         Only one of the pair, or a missing/unreadable/malformed file,"
    );
    println!(
        "                         is refused at startup. Rotate a certificate by draining and"
    );
    println!("                         restarting with the new pair.");
    println!(
        "  --behind-proxy         Serve WebSocket without TLS, for a reverse proxy that ends TLS"
    );
    println!(
        "                         in front of the relay (env CRETLINK_RELAY_BEHIND_PROXY=1). Not"
    );
    println!(
        "                         together with --tls-cert/--tls-key. The relay does not encrypt"
    );
    println!("                         the connection from the proxy: keep it on this host or a");
    println!("                         private network.");
    println!("  -V, --version          Print the version and exit.");
    println!("  -h, --help             Show this help.");
}

/// Prints the final metadata-only metrics snapshot (REQ-S-017): counts and an
/// active gauge, never a credential, ticket, or forwarded byte of content.
fn print_metrics(m: &MetricsSnapshot) {
    println!("Relay metrics at shutdown:");
    println!("  sessions admitted: {}", m.sessions_admitted);
    println!("  sessions ended:    {}", m.sessions_ended);
    println!("  sessions active:   {}", m.sessions_active);
    println!("  bytes forwarded:   {}", m.bytes_forwarded);
    println!("  frames forwarded:  {}", m.frames_forwarded);
    println!(
        "  denied:            auth {}, rate-limit {}, quota {}, unavailable {}",
        m.denied_auth, m.denied_rate_limit, m.denied_quota, m.denied_unavailable
    );
}
