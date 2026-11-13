//! What the tests of the relay share (ST-M007-011, ST-M007-013): a folder of
//! their own, an install in a home of its own, and the real relay server with
//! TLS inside the test process.
//!
//! The relay's certificate and key are minted for the run
//! ([`cretlink_wss::testcert`]); no key material is committed (REQ-N-017). The
//! certificate is written as the PEM file a relay profile names, which is how
//! a technician with a certificate authority of their own sets a copy up.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use cretlink_cli::install::{BaseDirs, Layout, Platform};
use cretlink_relay::{AdmissionControl, AdmissionPolicy, ControlConfig, Rendezvous, StopSignal};
use cretlink_relay_server::{Leg, ServeConfig, serve_wss};
use cretlink_wss::WssServerStream;
use tokio::net::{TcpListener, TcpStream};

use super::ThisCopy;

/// The name the test relay's certificate carries. A profile for the test
/// relay reaches it by its loopback address and gives this as `name`.
pub(crate) const RELAY_NAME: &str = "relay.test";

/// The file name the tests give the relay's certificate next to a profile.
pub(crate) const CA_FILE: &str = "relay-ca.pem";

static NEXT_FOLDER: AtomicU64 = AtomicU64::new(0);

/// A folder of its own for one test, removed afterwards.
#[derive(Debug)]
pub(crate) struct Folder(pub(crate) PathBuf);

impl Folder {
    pub(crate) fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cretlink-app-relay-{test}-{}-{}",
            std::process::id(),
            NEXT_FOLDER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    pub(crate) fn write(&self, name: &str, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Every file under the folder with its content, to show that something
    /// read and wrote nothing.
    pub(crate) fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    out.push((path.clone(), Vec::new()));
                    walk(&path, out);
                } else {
                    let content = std::fs::read(&path).unwrap();
                    out.push((path, content));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.0, &mut out);
        out
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A technician copy in a home of its own: where its program is, and the
/// layout of an install in that home.
#[derive(Debug)]
pub(crate) struct Home {
    pub(crate) folder: Folder,
    pub(crate) layout: Layout,
}

impl Home {
    pub(crate) fn new(test: &str) -> Self {
        let folder = Folder::new(test);
        let home = folder.path("home");
        let dirs = BaseDirs {
            home: home.clone(),
            local_app_data: Some(home.join("AppData").join("Local")),
            app_data: Some(home.join("AppData").join("Roaming")),
            data_home: None,
        };
        let layout = Layout::new(Platform::current(), &dirs).unwrap();
        Self { folder, layout }
    }

    /// The installed copy of this home: it runs from the install location
    /// and keeps settings.
    pub(crate) fn installed(&self) -> ThisCopy {
        ThisCopy {
            exe: self.layout.exe.clone(),
            installed: Some(self.layout.clone()),
        }
    }

    /// A copy that runs once from the downloads folder of this home.
    pub(crate) fn run_once(&self) -> ThisCopy {
        ThisCopy {
            exe: self.folder.path("home/Downloads/cretlink"),
            installed: None,
        }
    }
}

/// A certificate as the text of a PEM file.
pub(crate) fn pem(der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// The real relay server with TLS on a loopback port, inside the test
/// process.
pub(crate) struct TestRelay {
    /// Where it listens.
    pub(crate) addr: SocketAddr,
    /// Its certificate, as the text of a PEM file.
    pub(crate) cert_pem: String,
    /// Its admission gate, to read its counters.
    pub(crate) control: AdmissionControl,
    /// Keeps the relay serving: when it is dropped, the relay stops.
    signal: StopSignal,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for TestRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestRelay")
            .field("addr", &self.addr)
            .finish()
    }
}

impl TestRelay {
    /// Starts a relay that admits by `policy`, with a certificate for
    /// [`RELAY_NAME`].
    pub(crate) async fn start(policy: AdmissionPolicy) -> Self {
        Self::start_with(policy, ServeConfig::default()).await
    }

    /// Like [`TestRelay::start`], with other timing.
    pub(crate) async fn start_with(policy: AdmissionPolicy, config: ServeConfig) -> Self {
        let (cert, key) = cretlink_wss::testcert::self_signed(RELAY_NAME).unwrap();
        let cert_pem = pem(cert.as_ref());
        let control = AdmissionControl::new(policy, ControlConfig::public_default());
        let rendezvous: Arc<Rendezvous<Leg<WssServerStream<TcpStream>>>> =
            Arc::new(Rendezvous::new());
        let server_config = cretlink_wss::server_config(vec![cert], key).unwrap();
        let acceptor = cretlink_wss::acceptor(server_config);
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (signal, stop) = StopSignal::new();
        let task = tokio::spawn(serve_wss(
            listener,
            acceptor,
            control.clone(),
            rendezvous,
            config,
            stop,
        ));
        Self {
            addr,
            cert_pem,
            control,
            signal,
            task,
        }
    }

    /// Stops the relay as its operator would: it takes no new connection,
    /// and every session it is passing on is cut on both sides.
    pub(crate) fn stop(&self) {
        self.signal.stop();
    }

    /// Takes the relay away and answers once its port is closed, so that
    /// what dials it next finds nothing listening there.
    pub(crate) async fn gone(mut self) {
        self.task.abort();
        // The listener goes with the task that owns it.
        let _ = (&mut self.task).await;
    }

    /// The text of a profile file for this relay: its loopback address, the
    /// name in its certificate, its certificate as the authority file
    /// [`CA_FILE`], and `key` when the relay asks for one.
    pub(crate) fn profile_text(&self, key: Option<&str>) -> String {
        let key = key.map_or(String::new(), |key| format!("key = \"{key}\"\n"));
        format!(
            "[relay]\naddress = \"{}\"\nname = \"{RELAY_NAME}\"\nca = \"{CA_FILE}\"\n{key}",
            self.addr
        )
    }

    /// Puts a profile file for this relay, with its certificate file, next to
    /// a program in `folder`, and answers that program's path.
    pub(crate) fn set_up(&self, folder: &Folder, key: Option<&str>) -> PathBuf {
        folder.write(CA_FILE, &self.cert_pem);
        folder.write("cretlink.toml", self.profile_text(key));
        folder.path("CretLink-Support.exe")
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}
