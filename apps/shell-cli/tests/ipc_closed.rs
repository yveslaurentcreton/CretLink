//! Fail-closed over the real same-user IPC transport (ST-M008-002).
//!
//! [`engine.rs`](engine.rs) proves the client's decision logic against a
//! scripted fake console over an in-memory pipe. This test closes the loop over
//! the **genuine** local IPC transport (named pipe / Unix socket): a real
//! [`IpcServer`] backed by the [`ClosedRoute`] — the console's idle state with no
//! consented, attended session — answers the client's handshake, and the engine
//! must refuse to open a terminal, failing closed with
//! [`SessionError::NoConsentedSession`] (REQ-S-009). The CLI is thus a plain
//! third same-user consumer beside the MCP bridge and the console panel: it
//! borrows the routed session's authority and can create none of its own.
//!
//! `unwrap`/`expect`/`panic` are allowed here: this is test-only code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cretlink_console::{ClosedRoute, ConsoleRoute, IpcAudit, IpcServer, NoIpcAudit};
use cretlink_mcp::ipc::connect;
use cretlink_mcp::{IpcEndpoint, IpcListener, IpcStream};
use cretlink_protocol::Shell;
use cretlink_shell::engine::{self, OpenConfig, SessionError};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

static ENDPOINT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_endpoint() -> IpcEndpoint {
    let n = ENDPOINT_COUNTER.fetch_add(1, Ordering::SeqCst);
    IpcEndpoint::named(format!(
        "cretlink-shell-closed-{}-{}",
        std::process::id(),
        n
    ))
}

fn spawn_server(
    route: Arc<dyn ConsoleRoute>,
    audit: Arc<dyn IpcAudit>,
) -> (IpcEndpoint, JoinHandle<()>, oneshot::Sender<()>) {
    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let in_flight = Arc::new(AtomicU64::new(0));
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(IpcServer::serve(listener, route, audit, in_flight, rx));
    (endpoint, task, tx)
}

async fn connect_client(endpoint: &IpcEndpoint) -> IpcStream {
    for _ in 0..100 {
        if let Ok(stream) = connect(endpoint).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("could not connect to the IPC endpoint");
}

/// With no consented session on the console, the engine refuses to open a
/// terminal over the real IPC transport and fails closed — never fabricating a
/// session of its own (REQ-S-009, REQ-S-023).
#[tokio::test]
async fn no_consented_session_fails_closed_over_real_ipc() {
    let (endpoint, server, _shutdown) = spawn_server(Arc::new(ClosedRoute), Arc::new(NoIpcAudit));
    let mut stream = connect_client(&endpoint).await;

    let cfg = OpenConfig {
        shell: Shell::PowerShell,
        cols: 80,
        rows: 24,
        working_dir: None,
        env: BTreeMap::new(),
    };
    let err = engine::open_and_attach(&mut stream, &cfg)
        .await
        .unwrap_err();
    assert_eq!(err, SessionError::NoConsentedSession);

    // Tearing down: dropping the shutdown sender stops the server task.
    drop(stream);
    drop(_shutdown);
    let _ = server.await;
}
