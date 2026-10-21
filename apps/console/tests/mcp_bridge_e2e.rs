//! Full-chain MCP bridge end-to-end test (ST-M004-001/002/004, REQ-F-016,
//! REQ-S-009).
//!
//! The whole stack is wired over **real local IPC** and a **loopback TCP**
//! session:
//!
//! ```text
//! rmcp client <-duplex-> McpBridge -> IpcTargetSession
//!     <-named pipe / unix socket-> IpcServer -> ConsoleSessionService
//!         -> HostHandle <-loopback TCP-> SessionJoiner (real broker)
//! ```
//!
//! A real [`SessionHost`] is paired over loopback TCP to a [`SessionJoiner`]
//! presenting a *simulated* target identity (hostname `SIMULATED-TARGET`,
//! `simulated = true`) and running the real execution broker, so a portable
//! `echo` exercises the genuine exec path. Consent is granted, the console runs
//! its IPC server, and a real rmcp client drives the bridge over an in-process
//! stdio transport.
//!
//! The `cretlink-simulator` wire peer predates full-control consent (it rejects
//! `Control` frames), so the consented-session lifecycle this test needs is
//! realized with `SessionJoiner` + the real broker and a portable command — the
//! alternative the task blesses ("or a portable echo").
//!
//! Assertions: the client lists the full generic surface (five exec-era tools,
//! ten file and transfer tools, and nine visual desktop primitives);
//! `remote_system_info` returns the simulated target identity; `remote_exec`
//! streams output and reports `Completed`; every tool result carries the routed
//! session id and target (REQ-F-016). Negative: with no consented session the
//! tools fail closed and nothing runs — including the desktop surface, where a
//! fail-closed capture carries no image; after a revoke, both an identity query
//! and an actual `remote_exec` fail closed (ST-M004-004, REQ-S-009).
//!
//! # Scope (what this does NOT cover)
//!
//! This drives `McpBridge` through an in-process rmcp client over
//! `tokio::io::duplex`, so it exercises the whole console-side chain over real
//! local IPC but does **not** spawn the shipped `cretlink-mcp` binary. The
//! binary's real `stdio()` transport, `IpcEndpoint::user_default()`, panic hook,
//! and stderr-only banners are built but not run here, and the ADR-0004
//! stdout-purity consequence (the binary emits nothing but JSON-RPC frames on
//! stdout) is not asserted. Genuine spawn-by-a-real-client interop, OS-level
//! cross-user IPC rejection, and stdout-frame purity of the shipped binary are
//! left to the attended real-client test (ST-M004-005).
//!
//! `unwrap`/`expect`/`panic` are allowed here: test-only code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cretlink_console::{
    ChannelAudit, ClosedRoute, ConsoleRoute, ConsoleSessionService, IpcAudit, IpcServer,
};
use cretlink_mcp::{ExecParams, IpcEndpoint, IpcListener, IpcTargetSession, McpBridge};
use cretlink_protocol::{PROTOCOL_V1, SessionId, TargetIdentity};
use cretlink_session::{
    ConsoleHints, ExecRequest, HostHandle, JoinHandle, Origin, RevokeReason, SessionEvent,
    SessionHost, SessionJoiner, Shell,
};
use cretlink_shared::{Actor, AuditValue};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, serve_client, serve_server};
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle as TokioTask;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(5);

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// A simulated-looking, consent-bound identity: `simulated = true`, hostname
/// `SIMULATED-TARGET`, so a simulated result can never be mistaken for a real
/// customer computer (REQ-F-016).
fn simulated_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "SIMULATED-TARGET".to_string(),
        os_version: "Simulated Windows".to_string(),
        arch: "x86_64".to_string(),
        elevated: false,
        simulated: true,
        agent_version: "e2e-agent".to_string(),
    }
}

/// A process-unique IPC endpoint so parallel tests never collide.
fn unique_endpoint() -> IpcEndpoint {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    IpcEndpoint::named(format!("cretlink-e2e-{}-{n}", std::process::id()))
}

async fn next_event(handle: &mut HostHandle) -> SessionEvent {
    timeout(STEP, handle.next_event())
        .await
        .expect("timed out waiting for a session event")
        .expect("session event stream ended unexpectedly")
}

/// Binds a console on loopback, dials it with an agent presenting
/// `simulated_identity`, grants consent, and returns the active handle plus the
/// live agent handle (kept alive to hold the session open).
async fn active_session() -> (HostHandle, JoinHandle, SessionId, TargetIdentity) {
    let hints = ConsoleHints {
        label: "This computer (technician)".to_string(),
    };
    let mut host = SessionHost::host(loopback(), hints, PROTOCOL_V1, Vec::new())
        .await
        .expect("bind console");
    let addr = host
        .local_addr()
        .expect("a LAN-bound console reports its local listener address");
    let mut joiner = SessionJoiner::join(addr, simulated_identity(), PROTOCOL_V1, Vec::new())
        .await
        .expect("dial agent");

    assert_eq!(next_event(&mut host).await, SessionEvent::Connected);
    assert_eq!(
        next_event(&mut host).await,
        SessionEvent::AwaitingApprovalCompare
    );
    host.confirm_code().expect("confirm the approval code");
    assert_eq!(next_event(&mut host).await, SessionEvent::AwaitingConsent);
    joiner.grant_consent().expect("grant consent");

    let identity = match next_event(&mut host).await {
        SessionEvent::ConsentGranted { identity } => identity,
        other => panic!("expected ConsentGranted, got {other:?}"),
    };
    assert_eq!(next_event(&mut host).await, SessionEvent::Active);
    let session = host.session().expect("session id once active");
    // Drain the joiner's Connected/AwaitingApprovalCompare/AwaitingConsent so its
    // bounded event buffer never backs up while the test runs (agent output flows
    // over the wire, not this local stream).
    let _ = timeout(STEP, joiner.next_event()).await;
    (host, joiner, session, identity)
}

/// Spawns the bridge over one duplex half and returns a connected rmcp client.
async fn bridge_client(endpoint: IpcEndpoint) -> RunningService<RoleClient, ()> {
    let target = IpcTargetSession::connect(endpoint).await;
    let bridge = McpBridge::new(Arc::new(target));
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        if let Ok(running) = serve_server(bridge, server_io).await {
            let _ = running.waiting().await;
        }
    });
    serve_client((), client_io)
        .await
        .expect("rmcp client initialization")
}

fn structured(result: &rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("tool result carries structured content")
}

fn exec_args(command: &str) -> Map<String, Value> {
    let mut args = Map::new();
    // `cmd` maps to the portable `sh -c` on non-Windows and `cmd /c` on Windows
    // via the broker, so `echo` behaves the same on every CI OS.
    args.insert("shell".to_string(), json!("cmd"));
    args.insert("command".to_string(), json!(command));
    args
}

/// Positive path plus the after-revoke fail-closed case (ST-M004-004).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_chain_lists_tools_runs_exec_and_fails_closed_after_revoke() {
    let (host, joiner, session, identity) = active_session().await;
    let (service, _driver): (ConsoleSessionService, TokioTask<()>) =
        ConsoleSessionService::start(host, session, identity.clone());

    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let (channel_audit, audit_rx) = ChannelAudit::bounded(1024);
    let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
    let audit: Arc<dyn IpcAudit> = Arc::new(channel_audit);
    let in_flight = service.activity_counter();
    // Keep the shutdown sender alive: dropping it asks the server to stop.
    let (_ipc_shutdown_tx, ipc_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(IpcServer::serve(
        listener,
        route,
        audit,
        in_flight,
        ipc_shutdown_rx,
    ));

    let client = bridge_client(endpoint).await;

    // The client discovers the full generic surface: the five exec-era tools,
    // the ten file and transfer tools (ST-M004-003), and the nine visual desktop
    // primitives (ST-M005-004).
    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    for expected in [
        "remote_system_info",
        "remote_exec",
        "remote_write_stdin",
        "remote_cancel",
        "remote_list_jobs",
        "remote_stat",
        "remote_list_dir",
        "remote_make_dir",
        "remote_move",
        "remote_delete",
        "remote_remove_dir",
        "remote_upload",
        "remote_download",
        "remote_list_staged",
        "remote_cleanup",
        "remote_capture_desktop",
        "remote_desktop_pointer",
        "remote_desktop_scroll",
        "remote_desktop_key",
        "remote_desktop_type",
        "remote_desktop_hotkey",
        "remote_desktop_wait",
        "remote_clipboard_get",
        "remote_clipboard_set",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing tool {expected}"
        );
    }

    // remote_system_info returns the simulated target identity, embedding the
    // routed session id and target (REQ-F-016).
    let info = client
        .call_tool(CallToolRequestParams::new("remote_system_info"))
        .await
        .expect("remote_system_info");
    assert_ne!(info.is_error, Some(true));
    let body = structured(&info);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["target"]["simulated"], json!(true));
    assert_eq!(body["target"]["hostname"], json!("SIMULATED-TARGET"));

    // remote_exec runs a portable echo end to end and returns streamed output
    // with a Completed outcome; the result carries the routed identity.
    let exec = client
        .call_tool(
            CallToolRequestParams::new("remote_exec").with_arguments(exec_args("echo hello")),
        )
        .await
        .expect("remote_exec");
    assert_ne!(exec.is_error, Some(true));
    let body = structured(&exec);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["target"]["hostname"], json!("SIMULATED-TARGET"));
    assert_eq!(body["result"]["outcome"], json!("completed"));
    let stdout = BASE64
        .decode(
            body["result"]["stdout_base64"]
                .as_str()
                .expect("stdout_base64"),
        )
        .expect("valid base64");
    assert!(
        String::from_utf8_lossy(&stdout).contains("hello"),
        "stdout was {:?}",
        String::from_utf8_lossy(&stdout)
    );

    // A locally rejected file operation (an adversarial path passes the
    // bridge's length checks but fails the console-side protocol validation
    // in session_to_target's chain) reports its TRUE typed class through the
    // structured envelope: invalid_path, never a collapsed limit_exceeded
    // (ST-M003-003, REQ-N-015).
    let invalid = client
        .call_tool(
            CallToolRequestParams::new("remote_stat").with_arguments(
                [(
                    "path".to_string(),
                    json!({ "kind": "absolute", "path": "C:\\Users\\..\\Windows" }),
                )]
                .into_iter()
                .collect::<Map<String, Value>>(),
            ),
        )
        .await
        .expect("remote_stat returns a result envelope");
    assert_eq!(invalid.is_error, Some(true));
    let body = structured(&invalid);
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error_kind"], json!("remote"));
    assert_eq!(body["error_code"], json!("invalid_path"));

    // Each routed tool call was audited with the routed session, actor Mcp,
    // and correlation args (job id + command, ST-M003-006).
    let mut audited_exec = false;
    while let Ok(entry) = audit_rx.try_recv() {
        if entry.action == "mcp_remote_exec" {
            assert_eq!(entry.actor, Actor::Mcp);
            assert_eq!(entry.session, Some(session));
            assert!(
                entry.args.0.contains_key("job_id"),
                "exec audit entry must carry the job id (ST-M003-006)"
            );
            assert!(
                entry.args.0.contains_key("command"),
                "exec audit entry must carry the command text (ST-M003-006)"
            );
            audited_exec = true;
        }
    }
    assert!(
        audited_exec,
        "the routed remote_exec call must be audited (REQ-S-017)"
    );

    // Revoke: the console flips fail-closed at once. A later tool call must fail
    // closed and nothing runs (ST-M004-004, REQ-S-009).
    service.revoke(RevokeReason::TechnicianRequested);
    let after = client
        .call_tool(CallToolRequestParams::new("remote_system_info"))
        .await
        .expect("tool call still returns a result after revoke");
    assert_eq!(
        after.is_error,
        Some(true),
        "a revoked session must fail closed"
    );
    let body = structured(&after);
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error_kind"], json!("no_consented_session"));

    // And an actual EXECUTION — not just an identity query — must be refused
    // after revoke, proving no remote work runs on a revoked session
    // (ST-M004-004: revocation invalidates outstanding authority immediately).
    let exec_after = client
        .call_tool(
            CallToolRequestParams::new("remote_exec")
                .with_arguments(exec_args("echo should-not-run")),
        )
        .await
        .expect("exec tool call still returns a result after revoke");
    assert_eq!(
        exec_after.is_error,
        Some(true),
        "a revoked session must refuse execution"
    );
    assert_eq!(
        structured(&exec_after)["error_kind"],
        json!("no_consented_session")
    );

    client.cancel().await.ok();
    server.abort();
    drop(joiner);
}

/// With no consented session at all, every tool fails closed and nothing runs
/// (REQ-S-009).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_consented_session_fails_closed() {
    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let route: Arc<dyn ConsoleRoute> = Arc::new(ClosedRoute);
    let audit: Arc<dyn IpcAudit> = Arc::new(cretlink_console::NoIpcAudit);
    let in_flight = Arc::new(AtomicU64::new(0));
    // Keep the shutdown sender alive: dropping it asks the server to stop.
    let (_ipc_shutdown_tx, ipc_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(IpcServer::serve(
        listener,
        route,
        audit,
        in_flight,
        ipc_shutdown_rx,
    ));

    let client = bridge_client(endpoint).await;

    // Tools are still discoverable (the bridge stays up), but every call fails
    // closed with the explicit no-session error. Five generic + ten file and
    // transfer + nine visual desktop primitives (ST-M005-004).
    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(tools.len(), 24);

    let info = client
        .call_tool(CallToolRequestParams::new("remote_system_info"))
        .await
        .expect("call returns a result");
    assert_eq!(info.is_error, Some(true));
    assert_eq!(
        structured(&info)["error_kind"],
        json!("no_consented_session")
    );

    let exec = client
        .call_tool(CallToolRequestParams::new("remote_exec").with_arguments(exec_args("echo nope")))
        .await
        .expect("call returns a result");
    assert_eq!(exec.is_error, Some(true));
    assert_eq!(
        structured(&exec)["error_kind"],
        json!("no_consented_session")
    );

    // The file and transfer surface must fail closed too: a no-argument
    // inspection and an argument-bearing transfer both refuse (REQ-S-009).
    let staged = client
        .call_tool(CallToolRequestParams::new("remote_list_staged"))
        .await
        .expect("call returns a result");
    assert_eq!(staged.is_error, Some(true));
    assert_eq!(
        structured(&staged)["error_kind"],
        json!("no_consented_session")
    );

    let mut upload_args = serde_json::Map::new();
    upload_args.insert("local_path".to_string(), json!("C:\\tech\\x.txt"));
    upload_args.insert("dest_name".to_string(), json!("x.txt"));
    upload_args.insert("source_label".to_string(), json!("tech:x"));
    upload_args.insert("cleanup".to_string(), json!("on_session_end"));
    let upload = client
        .call_tool(CallToolRequestParams::new("remote_upload").with_arguments(upload_args))
        .await
        .expect("call returns a result");
    assert_eq!(upload.is_error, Some(true));
    assert_eq!(
        structured(&upload)["error_kind"],
        json!("no_consented_session")
    );

    // The visual desktop surface must fail closed identically: neither a
    // screenshot nor an input injection is served without a consented session
    // (ST-M005-004, REQ-S-009). A capture that fails closed can carry no image.
    let mut capture_args = serde_json::Map::new();
    capture_args.insert("display".to_string(), json!(0));
    capture_args.insert("format".to_string(), json!("png"));
    let capture = client
        .call_tool(
            CallToolRequestParams::new("remote_capture_desktop").with_arguments(capture_args),
        )
        .await
        .expect("call returns a result");
    assert_eq!(capture.is_error, Some(true));
    assert_eq!(
        structured(&capture)["error_kind"],
        json!("no_consented_session")
    );
    assert!(
        !capture
            .content
            .iter()
            .any(|block| block.as_image().is_some()),
        "a fail-closed capture must not carry a fabricated image"
    );

    let mut pointer_args = serde_json::Map::new();
    pointer_args.insert("action".to_string(), json!("click"));
    pointer_args.insert("button".to_string(), json!("left"));
    pointer_args.insert("x".to_string(), json!(10));
    pointer_args.insert("y".to_string(), json!(10));
    let pointer = client
        .call_tool(
            CallToolRequestParams::new("remote_desktop_pointer").with_arguments(pointer_args),
        )
        .await
        .expect("call returns a result");
    assert_eq!(pointer.is_error, Some(true));
    assert_eq!(
        structured(&pointer)["error_kind"],
        json!("no_consented_session")
    );

    client.cancel().await.ok();
    server.abort();
}

/// A path-traversal attempt in the constrained staging area, submitted through
/// the MCP surface, is rejected as `invalid_path` and never reaches the target
/// filesystem (ST-M003-007, ADR-0012, REQ-N-012, REQ-S-009). The bridge forwards
/// the staging component unchanged; the authoritative adversarial-path
/// validation on the console side refuses any staging name that is not a single
/// safe component, so a separator or a `..` cannot escape the staging root over
/// the tool surface. This is the staging-authority sibling of the absolute-path
/// traversal rejection asserted in
/// `full_chain_lists_tools_runs_exec_and_fails_closed_after_revoke`, and the
/// end-to-end MCP counterpart to the protocol-level `validate_staging_name` and
/// the store's own `staging_reference_cannot_escape_root` backstop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staging_path_traversal_is_rejected_at_the_mcp_surface() {
    let (host, joiner, session, identity) = active_session().await;
    let (service, _driver): (ConsoleSessionService, TokioTask<()>) =
        ConsoleSessionService::start(host, session, identity.clone());

    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
    let audit: Arc<dyn IpcAudit> = Arc::new(cretlink_console::NoIpcAudit);
    let in_flight = service.activity_counter();
    // Keep the shutdown sender alive: dropping it asks the server to stop.
    let (_ipc_shutdown_tx, ipc_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(IpcServer::serve(
        listener,
        route,
        audit,
        in_flight,
        ipc_shutdown_rx,
    ));

    let client = bridge_client(endpoint).await;

    // A backslash separator, a forward-slash separator, and a bare parent
    // reference: each is refused with the true typed class, never silently
    // forwarded to the target and never a panic.
    for bad in ["..\\escape.bin", "sub/escape.bin", ".."] {
        let result = client
            .call_tool(
                CallToolRequestParams::new("remote_stat").with_arguments(
                    [(
                        "path".to_string(),
                        json!({ "kind": "staging", "name": bad }),
                    )]
                    .into_iter()
                    .collect::<Map<String, Value>>(),
                ),
            )
            .await
            .expect("remote_stat returns a result envelope");
        assert_eq!(
            result.is_error,
            Some(true),
            "staging name {bad:?} must be refused, not resolved"
        );
        let body = structured(&result);
        assert_eq!(body["ok"], json!(false), "staging name {bad:?}");
        assert_eq!(
            body["error_code"],
            json!("invalid_path"),
            "staging traversal {bad:?} must surface its true typed class, got {body}"
        );
    }

    client.cancel().await.ok();
    server.abort();
    drop(joiner);
}

/// Reads agent-side events until the `CommandStarted` for `command` and returns
/// the [`Origin`] the agent saw on the wire — the attribution the customer-facing
/// surface renders (ST-M004-006).
async fn next_command_origin(joiner: &mut JoinHandle, command: &str) -> Origin {
    loop {
        let event = timeout(STEP, joiner.next_event())
            .await
            .expect("agent event before timeout")
            .expect("agent event stream stayed open");
        if let SessionEvent::CommandStarted {
            command: seen,
            origin,
            ..
        } = event
            && seen == command
        {
            return origin;
        }
    }
}

/// Origin (ST-M004-006): the SAME operation reaches the agent with a
/// distinguishable origin depending on whether the technician console (the CLI
/// path) or the AI (the MCP/bridge path) drove it, so the attended customer
/// surface can label AI-driven work distinctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn console_and_bridge_paths_reach_the_agent_with_distinct_origin() {
    let (host, mut joiner, session, identity) = active_session().await;
    let (service, _driver): (ConsoleSessionService, TokioTask<()>) =
        ConsoleSessionService::start(host, session, identity);

    // Console path (the interactive CLI): technician-driven.
    service
        .run_command(ExecRequest::new(Shell::Cmd, "echo from-console"))
        .await
        .expect("console-path run_command");
    assert_eq!(
        next_command_origin(&mut joiner, "echo from-console").await,
        Origin::Console,
        "a command from the console path must reach the agent as console origin"
    );

    // Bridge path (the MCP-facing collected exec): AI-driven.
    let _ = service
        .exec(
            ExecParams {
                shell: Shell::Cmd,
                command: "echo from-ai".to_string(),
                working_dir: None,
                env: std::collections::BTreeMap::new(),
                timeout_ms: None,
                sensitive: false,
            },
            64 * 1024,
        )
        .await
        .expect("bridge-path exec");
    assert_eq!(
        next_command_origin(&mut joiner, "echo from-ai").await,
        Origin::Mcp,
        "the same command from the bridge path must reach the agent as mcp origin"
    );

    drop(service);
    drop(joiner);
}

/// Activity correlation (ST-M004-006): a routed bridge call's audit record and
/// its structured tool result carry the same console-minted `call_id`, the exec
/// audit record shares the tool result's job id, and an upload's audit digest
/// equals the digest reported to the caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_record_and_tool_result_share_call_id_and_correlation() {
    let (host, joiner, session, identity) = active_session().await;
    let (service, _driver): (ConsoleSessionService, TokioTask<()>) =
        ConsoleSessionService::start(host, session, identity.clone());

    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let (channel_audit, audit_rx) = ChannelAudit::bounded(1024);
    let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
    let audit: Arc<dyn IpcAudit> = Arc::new(channel_audit);
    let in_flight = service.activity_counter();
    let (_ipc_shutdown_tx, ipc_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(IpcServer::serve(
        listener,
        route,
        audit,
        in_flight,
        ipc_shutdown_rx,
    ));
    let client = bridge_client(endpoint).await;

    // exec: the result carries a non-zero call_id and the agent-assigned job id.
    let exec = client
        .call_tool(CallToolRequestParams::new("remote_exec").with_arguments(exec_args("echo hi")))
        .await
        .expect("remote_exec");
    let body = structured(&exec);
    let exec_call_id = body["call_id"]
        .as_u64()
        .expect("call_id in the exec result");
    let exec_job = body["result"]["job"]
        .as_u64()
        .expect("job in the exec result");
    assert!(exec_call_id > 0, "a routed call must carry a real call_id");

    // upload: a small local file is staged; the result reports its digest.
    let dir = std::env::temp_dir().join(format!(
        "cretlink-corr-{}-{exec_call_id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let local = dir.join("payload.bin");
    std::fs::write(&local, b"correlation-bytes").expect("write payload");
    let mut upload_args = Map::new();
    upload_args.insert(
        "local_path".to_string(),
        json!(local.to_string_lossy().to_string()),
    );
    upload_args.insert("dest_name".to_string(), json!("payload.bin"));
    upload_args.insert("source_label".to_string(), json!("tech:payload"));
    upload_args.insert("cleanup".to_string(), json!("on_session_end"));
    let upload = client
        .call_tool(CallToolRequestParams::new("remote_upload").with_arguments(upload_args))
        .await
        .expect("remote_upload");
    let up = structured(&upload);
    assert_eq!(
        up["result"]["outcome"],
        json!("completed"),
        "upload body {up}"
    );
    let upload_call_id = up["call_id"]
        .as_u64()
        .expect("call_id in the upload result");
    let digest_hex = up["result"]["digest_hex"]
        .as_str()
        .expect("digest_hex in the upload result")
        .to_string();

    // A FAILED routed call must correlate too (the case where correlation
    // matters most): an adversarial path is rejected as a remote invalid_path,
    // and the error result carries the same call_id the audit record was written
    // under (ST-M004-006).
    let failed = client
        .call_tool(
            CallToolRequestParams::new("remote_delete").with_arguments(
                [(
                    "path".to_string(),
                    json!({ "kind": "absolute", "path": "C:\\Users\\..\\Windows" }),
                )]
                .into_iter()
                .collect::<Map<String, Value>>(),
            ),
        )
        .await
        .expect("remote_delete returns a result envelope");
    assert_eq!(failed.is_error, Some(true));
    let fb = structured(&failed);
    assert_eq!(fb["error_kind"], json!("remote"));
    let failed_call_id = fb["call_id"]
        .as_u64()
        .expect("a failed routed call still carries its call_id");
    assert!(failed_call_id > 0);

    // The audit trail correlates one-to-one with the tool results.
    let mut exec_ok = false;
    let mut upload_ok = false;
    let mut failed_ok = false;
    while let Ok(entry) = audit_rx.try_recv() {
        match entry.action {
            "mcp_remote_exec" => {
                assert_eq!(audit_u64(&entry, "call_id"), exec_call_id);
                assert_eq!(audit_u64(&entry, "job_id"), exec_job);
                exec_ok = true;
            }
            "mcp_remote_upload" => {
                assert_eq!(audit_u64(&entry, "call_id"), upload_call_id);
                assert_eq!(
                    entry.digests.get("sha256").cloned().unwrap_or_default(),
                    digest_hex,
                    "the upload's audit digest must equal the digest reported to the caller"
                );
                upload_ok = true;
            }
            "mcp_remote_delete" => {
                assert_eq!(
                    audit_u64(&entry, "call_id"),
                    failed_call_id,
                    "the failed call's audit record and error result must share a call_id"
                );
                assert_eq!(entry.outcome, cretlink_shared::Outcome::Failed);
                failed_ok = true;
            }
            _ => {}
        }
    }
    assert!(exec_ok, "the exec completion record must be audited");
    assert!(upload_ok, "the upload completion record must be audited");
    assert!(
        failed_ok,
        "the failed delete completion record must be audited"
    );

    let _ = std::fs::remove_dir_all(&dir);
    client.cancel().await.ok();
    server.abort();
    drop(joiner);
}

/// The `Int` value of an audit arg, or `0` when absent.
fn audit_u64(entry: &cretlink_console::AuditEntry, key: &str) -> u64 {
    match entry.args.0.get(key) {
        Some(AuditValue::Int(n)) => u64::try_from(*n).unwrap_or(0),
        _ => 0,
    }
}

/// Bilateral stop (ST-M004-006): a customer-side revoke while a bridge exec is
/// in flight makes the running call report honestly (never a fabricated
/// success), and every subsequent tool call fails closed with the explicit
/// no-consented-session error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn customer_side_revoke_mid_exec_reports_honestly_then_fails_closed() {
    let (host, joiner, session, identity) = active_session().await;
    let (service, _driver): (ConsoleSessionService, TokioTask<()>) =
        ConsoleSessionService::start(host, session, identity.clone());

    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
    let audit: Arc<dyn IpcAudit> = Arc::new(cretlink_console::NoIpcAudit);
    let in_flight = service.activity_counter();
    let (_ipc_shutdown_tx, ipc_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(IpcServer::serve(
        listener,
        route,
        audit,
        in_flight,
        ipc_shutdown_rx,
    ));
    let client = Arc::new(bridge_client(endpoint).await);

    // `sort` reads stdin until EOF; the broker holds stdin open, so it hangs
    // until the session is torn down — a portable, deterministic in-flight job.
    let exec_client = Arc::clone(&client);
    let exec_task = tokio::spawn(async move {
        exec_client
            .call_tool(CallToolRequestParams::new("remote_exec").with_arguments(exec_args("sort")))
            .await
    });

    // Wait until the agent has actually started the job, so the revoke lands
    // mid-exec without a sleep-based race.
    let mut running = false;
    for _ in 0..100 {
        if let Ok(jobs) = service.exec_list_jobs().await
            && jobs.iter().any(|job| job.running)
        {
            running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(running, "the hanging job must be running before the revoke");
    // The console can see the AI actively driving, not merely connected.
    assert!(
        service.ai_in_flight() >= 1,
        "an in-flight AI exec must register on the activity counter"
    );

    // Customer-side stop: the agent (joiner) revokes.
    joiner
        .revoke(RevokeReason::CustomerRequested)
        .expect("customer-side revoke");

    // The interrupted call reports honestly — Cancelled or Unknown, never a
    // fabricated success.
    let interrupted = timeout(STEP, exec_task)
        .await
        .expect("in-flight exec returns after the revoke")
        .expect("exec task joined")
        .expect("tool result");
    let outcome = structured(&interrupted)["result"]["outcome"].clone();
    assert!(
        outcome == json!("unknown") || outcome == json!("cancelled"),
        "an interrupted exec must report an honest non-success outcome, got {outcome}"
    );

    // Every subsequent call fails closed with the explicit no-session error.
    let after = client
        .call_tool(CallToolRequestParams::new("remote_system_info"))
        .await
        .expect("returns a result after revoke");
    assert_eq!(after.is_error, Some(true));
    assert_eq!(
        structured(&after)["error_kind"],
        json!("no_consented_session")
    );
    let exec_after = client
        .call_tool(CallToolRequestParams::new("remote_exec").with_arguments(exec_args("echo nope")))
        .await
        .expect("returns a result after revoke");
    assert_eq!(exec_after.is_error, Some(true));
    assert_eq!(
        structured(&exec_after)["error_kind"],
        json!("no_consented_session")
    );

    // Once the calls have drained, the activity indicator returns to idle.
    assert_eq!(
        service.ai_in_flight(),
        0,
        "the in-flight counter must return to idle after the calls complete"
    );

    server.abort();
    drop(joiner);
    drop(client);
}

/// Bilateral stop, console side (ST-M004-006): the technician-side stop path
/// (`finish_local_revoke`, a different teardown than the customer/peer path)
/// must also make a mid-flight bridge exec report honestly and then fail closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn console_side_revoke_mid_exec_reports_honestly_then_fails_closed() {
    let (host, joiner, session, identity) = active_session().await;
    let (service, _driver): (ConsoleSessionService, TokioTask<()>) =
        ConsoleSessionService::start(host, session, identity.clone());

    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
    let audit: Arc<dyn IpcAudit> = Arc::new(cretlink_console::NoIpcAudit);
    let in_flight = service.activity_counter();
    let (_ipc_shutdown_tx, ipc_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(IpcServer::serve(
        listener,
        route,
        audit,
        in_flight,
        ipc_shutdown_rx,
    ));
    let client = Arc::new(bridge_client(endpoint).await);

    let exec_client = Arc::clone(&client);
    let exec_task = tokio::spawn(async move {
        exec_client
            .call_tool(CallToolRequestParams::new("remote_exec").with_arguments(exec_args("sort")))
            .await
    });

    let mut running = false;
    for _ in 0..100 {
        if let Ok(jobs) = service.exec_list_jobs().await
            && jobs.iter().any(|job| job.running)
        {
            running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(running, "the hanging job must be running before the stop");
    assert!(
        service.ai_in_flight() >= 1,
        "an in-flight AI exec must register"
    );

    // Console/technician-side stop.
    service.revoke(RevokeReason::TechnicianRequested);

    let interrupted = timeout(STEP, exec_task)
        .await
        .expect("in-flight exec returns after the stop")
        .expect("exec task joined")
        .expect("tool result");
    let outcome = structured(&interrupted)["result"]["outcome"].clone();
    assert!(
        outcome == json!("unknown") || outcome == json!("cancelled"),
        "an interrupted exec must report an honest non-success outcome, got {outcome}"
    );

    let after = client
        .call_tool(CallToolRequestParams::new("remote_system_info"))
        .await
        .expect("returns a result after stop");
    assert_eq!(after.is_error, Some(true));
    assert_eq!(
        structured(&after)["error_kind"],
        json!("no_consented_session")
    );

    server.abort();
    drop(joiner);
    drop(client);
}
