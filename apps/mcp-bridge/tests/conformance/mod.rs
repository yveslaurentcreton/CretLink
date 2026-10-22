//! Client-neutral stdio conformance harness plus the positive full-chain test
//! for the shipped `cretlink-mcp` binary (ST-M004-005; REQ-F-014, REQ-F-015,
//! REQ-N-001, REQ-N-010, REQ-N-020; ADR-0002 client matrix; ADR-0004 stdio
//! transport and its stdout-purity consequence).
//!
//! Every test here spawns the **real** shipped binary — `cretlink-mcp`, or
//! `cretlink-cli mcp` when included from `apps/cli` (ST-M009-002); the including
//! test crate names it in `BRIDGE_BIN` and `BRIDGE_ARGS` — and speaks **raw** JSON-RPC 2.0 over
//! its stdin/stdout with `serde_json` — deliberately **without any rmcp client
//! types** — to model an independently implemented MCP client such as Codex CLI.
//! The existing in-process tests (`crates/mcp`, `apps/console`) drive the bridge
//! with the rmcp SDK on both ends, which is self-interop; this file is the first
//! test that exercises the genuine spawn-by-a-foreign-client path.
//!
//! # Framing
//!
//! rmcp's stdio transport frames each JSON-RPC message as **one line terminated
//! by `\n`** (newline-delimited JSON, not LSP `Content-Length`): its
//! `JsonRpcMessageCodec` encodes with `serde_json::to_writer` followed by a
//! single `b'\n'`, and decodes by splitting on `\n` (stripping an optional
//! trailing `\r` and a leading UTF-8 BOM, and ignoring empty lines). The harness
//! mirrors that framing exactly: it writes compact JSON + `\n` and reads one
//! line per frame.
//!
//! # Endpoint isolation
//!
//! Each spawn sets `CRETLINK_IPC_ENDPOINT` to a unique per-test name (the binary
//! override added for this story). The conformance tests point it at an endpoint
//! with **no console**, so a real console running on the developer's machine can
//! never satisfy — or be reached by — a fail-closed assertion. The positive
//! full-chain test points it at the endpoint its own in-test console binds.
//!
//! `unwrap`/`expect`/`panic` are allowed here: test-only code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cretlink_console::{ConsoleRoute, ConsoleSessionService, IpcAudit, IpcServer, NoIpcAudit};
use cretlink_mcp::{IpcEndpoint, IpcListener};
use cretlink_protocol::{PROTOCOL_V1, SessionId, TargetIdentity, limits};
use cretlink_session::{
    ConsoleHints, HostHandle, JoinHandle, SessionEvent, SessionHost, SessionJoiner,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle as TokioTask;
use tokio::time::timeout;

/// The exact, stable tool surface the bridge must expose: the five exec-era
/// tools (ST-M004-002) plus the ten file/transfer tools (ST-M004-003). Asserted
/// for exact equality — no subset, no superset — so any drift in the shipped
/// surface fails a test rather than silently changing what a client sees.
const EXPECTED_TOOLS: [&str; 24] = [
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
];

/// A generous per-I/O deadline. Every stdin write and stdout read is wrapped in
/// it so a hung or fail-slow binary fails the test instead of hanging CI
/// (ADR-0004 promises fail-closed connect, not a blocked handshake).
const OP: Duration = Duration::from_secs(30);

/// A process-unique endpoint name so parallel tests never collide, and so the
/// conformance tests never resolve to the developer's real console endpoint.
fn unique_endpoint_name(tag: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cretlink-conf-{tag}-{}-{n}", std::process::id())
}

/// A minimal raw JSON-RPC stdio client for the shipped binary. It holds no MCP
/// SDK types; it just frames JSON lines and records every stdout line for the
/// stdout-purity check.
struct RawClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    /// Every non-empty stdout line the binary emitted, for the purity assertion.
    lines: Vec<String>,
    next_id: i64,
}

impl RawClient {
    /// Spawns the shipped binary against `endpoint`, piping stdin/stdout and
    /// leaving stderr inherited (so its banner/diagnostics show on failure but
    /// can never contaminate the captured stdout stream).
    fn spawn(endpoint: &str) -> Self {
        let mut child = Command::new(super::BRIDGE_BIN)
            .args(super::BRIDGE_ARGS)
            .env("CRETLINK_IPC_ENDPOINT", endpoint)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the shipped cretlink-mcp binary");
        let stdin = child.stdin.take().expect("child stdin pipe");
        let stdout = child.stdout.take().expect("child stdout pipe");
        Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
            lines: Vec::new(),
            next_id: 0,
        }
    }

    /// Writes one JSON-RPC message as a single `\n`-terminated line.
    async fn send(&mut self, msg: &Value) {
        let mut line = serde_json::to_string(msg).expect("serialize json-rpc frame");
        line.push('\n');
        timeout(OP, self.stdin.write_all(line.as_bytes()))
            .await
            .expect("stdin write did not time out")
            .expect("write to child stdin");
        timeout(OP, self.stdin.flush())
            .await
            .expect("stdin flush did not time out")
            .expect("flush child stdin");
    }

    /// Reads the next non-empty stdout line, recording it for the purity check.
    /// Returns `None` at EOF.
    async fn recv_line(&mut self) -> Option<String> {
        loop {
            let mut buf = String::new();
            let n = timeout(OP, self.reader.read_line(&mut buf))
                .await
                .expect("stdout read did not time out")
                .expect("read from child stdout");
            if n == 0 {
                return None;
            }
            let trimmed = buf.trim_end_matches(['\n', '\r']).to_string();
            if trimmed.is_empty() {
                continue;
            }
            self.lines.push(trimmed.clone());
            return Some(trimmed);
        }
    }

    /// Sends a request with a fresh id and returns the matching response object.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let line = self.recv_line().await.expect("a response frame before EOF");
            let value: Value = serde_json::from_str(&line).expect("response is valid JSON");
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return value;
            }
            // An unrelated notification/response: keep reading for our id.
        }
    }

    /// Sends a notification (no id, no response expected).
    async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// Performs the MCP handshake at `protocol_version` and returns the raw
    /// `initialize` response, then sends `notifications/initialized`.
    async fn initialize(&mut self, protocol_version: &str) -> Value {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {},
                    "clientInfo": {"name": "cretlink-conformance", "version": "0.0.0"},
                }),
            )
            .await;
        self.notify("notifications/initialized", json!({})).await;
        response
    }

    /// Calls a tool and returns the raw JSON-RPC response object.
    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    /// Closes stdin (so the server loop sees EOF and the process exits), drains
    /// any remaining stdout into the recorded lines, waits for exit, and returns
    /// every stdout line the binary produced.
    async fn finish(self) -> Vec<String> {
        let RawClient {
            mut child,
            stdin,
            mut reader,
            mut lines,
            ..
        } = self;
        drop(stdin);
        loop {
            let mut buf = String::new();
            match timeout(OP, reader.read_line(&mut buf)).await {
                Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                Ok(Ok(_)) => {
                    let trimmed = buf.trim_end_matches(['\n', '\r']).to_string();
                    if !trimmed.is_empty() {
                        lines.push(trimmed);
                    }
                }
            }
        }
        let _ = timeout(OP, child.wait()).await;
        lines
    }
}

/// Asserts the ADR-0004 stdout-purity consequence: every line the binary wrote
/// to stdout parses as a JSON-RPC 2.0 frame and nothing else (no banner, no log,
/// no panic text).
fn assert_stdout_is_pure_jsonrpc(lines: &[String]) {
    assert!(
        !lines.is_empty(),
        "the binary produced no stdout frames at all"
    );
    for line in lines {
        let value: Value = serde_json::from_str(line).unwrap_or_else(|err| {
            panic!("non-JSON line on stdout (violates ADR-0004 stdout purity): {line:?}: {err}")
        });
        assert_eq!(
            value.get("jsonrpc").and_then(Value::as_str),
            Some("2.0"),
            "stdout frame is not a JSON-RPC 2.0 message: {line:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// B. Client-neutral conformance (no console attached to the endpoint).
// ---------------------------------------------------------------------------

/// `tools/list` returns exactly the twenty-four stable tools, each input schema
/// compiles as a real JSON Schema, and the shared `limits` bounds are present —
/// all proven against the shipped binary through a hand-rolled client, and the
/// whole exchange keeps stdout pure JSON-RPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_is_the_exact_twenty_four_and_every_schema_validates() {
    let mut client = RawClient::spawn(&unique_endpoint_name("list"));
    client.initialize("2026-07-28").await;

    let response = client.request("tools/list", json!({})).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list returns a tools array");

    // Exact-name snapshot: no subset, no superset.
    let mut names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name is a string"))
        .collect();
    names.sort_unstable();
    let mut expected: Vec<&str> = EXPECTED_TOOLS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the shipped tool surface drifted from the agreed twenty-four tools"
    );

    // Every advertised input schema compiles under a JSON-Schema validator, so
    // an independent client (Codex) can consume them.
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let schema = &tool["inputSchema"];
        assert!(
            jsonschema::validator_for(schema).is_ok(),
            "input schema for {name} is not a valid JSON Schema: {schema}"
        );
    }

    // Spot-check that the shared protocol `limits` bounds ride along in the
    // shipped schemas, so schema and runtime enforcement cannot drift.
    let schema_of = |name: &str| -> Value {
        tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} listed"))["inputSchema"]
            .clone()
    };
    let exec = schema_of("remote_exec");
    assert_eq!(
        exec["properties"]["command"]["maxLength"],
        json!(limits::MAX_COMMAND_BYTES),
        "remote_exec.command bound missing from the shipped schema"
    );
    assert_eq!(
        exec["properties"]["timeout_ms"]["maximum"],
        json!(limits::MAX_TIMEOUT_MS),
        "remote_exec.timeout_ms bound missing from the shipped schema"
    );
    let stdin = schema_of("remote_write_stdin");
    assert_eq!(
        stdin["properties"]["data_base64"]["maxLength"],
        json!(cretlink_mcp::MAX_STDIN_BASE64_CHARS),
        "remote_write_stdin.data_base64 chunk-derived bound missing from the shipped schema"
    );
    // The chunk-derived stdin bound must reflect the shared chunk limit.
    assert_eq!(
        cretlink_mcp::MAX_STDIN_BASE64_CHARS,
        limits::MAX_CHUNK_BYTES.div_ceil(3) * 4,
        "the stdin base64 bound must be derived from the shared MAX_CHUNK_BYTES"
    );
    // The visual desktop schemas carry the shared desktop bounds too, so the
    // ST-M005-004 surface cannot drift between schema and runtime enforcement.
    let type_schema = schema_of("remote_desktop_type");
    assert_eq!(
        type_schema["properties"]["text"]["maxLength"],
        json!(limits::MAX_DESKTOP_TEXT_BYTES),
        "remote_desktop_type.text bound missing from the shipped schema"
    );
    let capture = schema_of("remote_capture_desktop");
    assert_eq!(
        capture["properties"]["max_width"]["maximum"],
        json!(limits::MAX_DESKTOP_DIM),
        "remote_capture_desktop.max_width bound missing from the shipped schema"
    );
    let clipboard = schema_of("remote_clipboard_set");
    assert_eq!(
        clipboard["properties"]["text"]["maxLength"],
        json!(limits::MAX_CLIPBOARD_BYTES),
        "remote_clipboard_set.text bound missing from the shipped schema"
    );

    // Every tool acts on the target, so every tool requires a stated purpose
    // (REQ-F-026, ST-M009-007): a required, bounded string whose description
    // asks for a plain-language reason the customer will read.
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let schema = &tool["inputSchema"];
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} declares no required arguments: {schema}"))
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            required.contains(&"purpose"),
            "{name} must require a stated purpose: {schema}"
        );
        let purpose = &schema["properties"]["purpose"];
        assert_eq!(purpose["type"], json!("string"), "{name}: {purpose}");
        assert_eq!(purpose["minLength"], json!(1), "{name}: {purpose}");
        assert_eq!(
            purpose["maxLength"],
            json!(limits::MAX_PURPOSE_CHARS),
            "{name}: {purpose}"
        );
        let description = purpose["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("customer") && description.contains("plain-language"),
            "{name}: the purpose description must ask for a plain-language reason \
             the customer reads: {description:?}"
        );
    }

    let lines = client.finish().await;
    assert_stdout_is_pure_jsonrpc(&lines);
}

/// `initialize` succeeds across at least two protocol versions (the current
/// revision and an older one), each spawning a fresh binary — an independent
/// client on a different MCP revision must still get a usable session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_succeeds_across_two_protocol_versions() {
    for version in ["2026-07-28", "2025-06-18"] {
        let mut client = RawClient::spawn(&unique_endpoint_name("init"));
        let response = client.initialize(version).await;
        assert!(
            response.get("error").is_none(),
            "initialize at {version} returned a JSON-RPC error: {response}"
        );
        let result = &response["result"];
        assert!(
            result["protocolVersion"].is_string(),
            "initialize at {version} returned no negotiated protocolVersion: {response}"
        );
        assert!(
            result["serverInfo"]["name"].is_string(),
            "initialize at {version} returned no serverInfo.name: {response}"
        );
        assert!(
            result.get("capabilities").is_some(),
            "initialize at {version} returned no capabilities: {response}"
        );
        let lines = client.finish().await;
        assert_stdout_is_pure_jsonrpc(&lines);
    }
}

/// A `tools/call` against an endpoint with no console fails closed: the binary
/// returns an error tool result carrying `ok: false` and a fail-closed
/// `error_kind`, and nothing panics or reaches a target (REQ-S-009, ADR-0004).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_without_a_console_fails_closed() {
    let mut client = RawClient::spawn(&unique_endpoint_name("closed"));
    client.initialize("2026-07-28").await;

    let response = client
        .call_tool(
            "remote_system_info",
            json!({"purpose": "Checking which computer this is"}),
        )
        .await;
    let result = &response["result"];
    assert_eq!(
        result["isError"],
        json!(true),
        "a call with no console must be an error result: {response}"
    );
    let body = &result["structuredContent"];
    assert_eq!(
        body["ok"],
        json!(false),
        "the fail-closed envelope must carry ok: false: {response}"
    );
    let kind = body["error_kind"].as_str().unwrap_or_default();
    assert!(
        kind == "disconnected" || kind == "no_consented_session",
        "expected a fail-closed error_kind, got {kind:?}: {response}"
    );

    let lines = client.finish().await;
    assert_stdout_is_pure_jsonrpc(&lines);
}

/// A call without a valid stated purpose is refused by the shipped binary with
/// an actionable `invalid_params` error before anything is sent — here no
/// console is even attached, so a refusal that reached the IPC would surface
/// as a fail-closed tool result instead (REQ-F-026, ST-M009-007).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_without_a_valid_purpose_is_rejected_before_anything_is_sent() {
    let mut client = RawClient::spawn(&unique_endpoint_name("purpose"));
    client.initialize("2026-07-28").await;

    let too_long = "a".repeat(limits::MAX_PURPOSE_CHARS + 1);
    let cases = [
        ("missing", json!({"shell": "cmd", "command": "echo hi"})),
        (
            "empty",
            json!({"shell": "cmd", "command": "echo hi", "purpose": ""}),
        ),
        (
            "whitespace",
            json!({"shell": "cmd", "command": "echo hi", "purpose": "   "}),
        ),
        (
            "multi-line",
            json!({"shell": "cmd", "command": "echo hi", "purpose": "line one\nline two"}),
        ),
        (
            "too long",
            json!({"shell": "cmd", "command": "echo hi", "purpose": too_long}),
        ),
    ];
    for (case, arguments) in cases {
        let response = client.call_tool("remote_exec", arguments).await;
        assert_rejected_for_purpose(case, &response);
    }

    // A tool with no other argument needs a purpose too.
    let response = client.call_tool("remote_list_jobs", json!({})).await;
    assert_rejected_for_purpose("remote_list_jobs without purpose", &response);

    let lines = client.finish().await;
    assert_stdout_is_pure_jsonrpc(&lines);
}

/// Asserts a tool call was refused for its stated purpose before it reached the
/// console. rmcp reports a schema-level failure (a missing field) as an error
/// tool result and a bridge validation failure as a JSON-RPC invalid-params
/// error; either way the message names `purpose`, and no routed envelope (the
/// `ok`/`session_id` body every call that reached the console carries, even a
/// fail-closed one) is produced.
fn assert_rejected_for_purpose(case: &str, response: &Value) {
    let message = if let Some(error) = response.get("error") {
        assert_eq!(
            error["code"],
            json!(-32602),
            "{case}: expected invalid params, got {response}"
        );
        error["message"].as_str().unwrap_or_default().to_string()
    } else {
        let result = &response["result"];
        assert_eq!(
            result["isError"],
            json!(true),
            "{case}: a call without a valid purpose must fail: {response}"
        );
        assert!(
            result.get("structuredContent").is_none(),
            "{case}: a refused call must not carry a routed envelope: {response}"
        );
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        message.contains("purpose"),
        "{case}: the error must name the purpose argument: {message:?}"
    );
}

// ---------------------------------------------------------------------------
// C. Positive full chain: shipped binary -> IPC -> console -> broker.
// ---------------------------------------------------------------------------

/// Spawns the shipped binary against a **real** `IpcServer` + `ConsoleSessionService`
/// bound to the override endpoint, so one CI test covers the whole shipped path
/// `client -> binary -> IPC -> console -> broker`: `remote_system_info` returns
/// the routed simulated identity and `remote_exec` runs a portable `echo`
/// through the real execution broker (following the ST-M004-003 precedent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_chain_reaches_the_target_through_the_shipped_binary() {
    let (host, joiner, session, identity) = active_session().await;
    let (service, _driver): (ConsoleSessionService, TokioTask<()>) =
        ConsoleSessionService::start(host, session, identity.clone());

    let endpoint = IpcEndpoint::named(unique_endpoint_name("full"));
    let listener = IpcListener::bind(&endpoint).expect("bind the IPC endpoint");
    let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
    let audit: Arc<dyn IpcAudit> = Arc::new(NoIpcAudit);
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

    let mut client = RawClient::spawn(endpoint.name());
    client.initialize("2026-07-28").await;

    // remote_system_info: routed identity flows the whole chain back.
    let info = client
        .call_tool(
            "remote_system_info",
            json!({"purpose": "Checking which computer this is"}),
        )
        .await;
    let body = &info["result"]["structuredContent"];
    assert_ne!(
        info["result"]["isError"],
        json!(true),
        "system_info: {info}"
    );
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["result"]["target"]["simulated"], json!(true));
    assert_eq!(
        body["result"]["target"]["hostname"],
        json!("SIMULATED-TARGET")
    );

    // remote_exec: a portable echo runs on the real broker end to end.
    let exec = client
        .call_tool(
            "remote_exec",
            json!({
                "shell": "cmd",
                "command": "echo interop",
                "purpose": "Checking that commands run",
            }),
        )
        .await;
    let body = &exec["result"]["structuredContent"];
    assert_ne!(exec["result"]["isError"], json!(true), "exec: {exec}");
    assert_eq!(body["result"]["outcome"], json!("completed"));
    let stdout = BASE64
        .decode(
            body["result"]["stdout_base64"]
                .as_str()
                .expect("stdout_base64 present"),
        )
        .expect("valid base64");
    assert!(
        String::from_utf8_lossy(&stdout).contains("interop"),
        "exec stdout was {:?}",
        String::from_utf8_lossy(&stdout)
    );

    // The stated purpose crossed the whole shipped chain — bridge, IPC, console,
    // session protocol — and reached the agent beside the real command, so the
    // attended customer surface can show it (REQ-F-026, ST-M009-007).
    let mut joiner = joiner;
    let started = loop {
        match timeout(OP, joiner.next_event())
            .await
            .expect("agent event before timeout")
            .expect("agent event stream stayed open")
        {
            SessionEvent::CommandStarted {
                command,
                origin,
                purpose,
                ..
            } if command == "echo interop" => break (origin, purpose),
            _ => {}
        }
    };
    assert_eq!(started.0, cretlink_protocol::Origin::Mcp);
    assert_eq!(
        started.1.as_ref().map(cretlink_protocol::Purpose::as_str),
        Some("Checking that commands run")
    );

    let lines = client.finish().await;
    assert_stdout_is_pure_jsonrpc(&lines);

    server.abort();
    drop(joiner);
}

// ---------------------------------------------------------------------------
// Console setup helpers (mirrors apps/console/tests/mcp_bridge_e2e.rs).
// ---------------------------------------------------------------------------

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
        agent_version: "conformance-agent".to_string(),
    }
}

async fn next_event(handle: &mut HostHandle) -> SessionEvent {
    timeout(OP, handle.next_event())
        .await
        .expect("timed out waiting for a session event")
        .expect("session event stream ended unexpectedly")
}

/// Binds a console on loopback, dials it with an agent presenting
/// `simulated_identity`, grants consent, and returns the active handles.
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
    // bounded event buffer never backs up while the test runs.
    let _ = timeout(OP, joiner.next_event()).await;
    (host, joiner, session, identity)
}
