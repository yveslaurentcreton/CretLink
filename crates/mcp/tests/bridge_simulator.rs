//! Simulator-backed end-to-end MCP test — the M001 exit-criterion evidence
//! (ST-M001-003).
//!
//! The full stack is wired in-process:
//!
//! ```text
//! rmcp client <-duplex-> McpBridge -> SessionTarget -> SessionClient <-duplex-> Simulator
//! ```
//!
//! A real rmcp client (from the SDK) talks to [`cretlink_mcp::McpBridge`] over
//! a tokio duplex "stdio". The bridge is backed by the console
//! [`cretlink_console::SessionTarget`] adapter around a real
//! [`cretlink_console::SessionClient`] whose peer is
//! [`cretlink_simulator::Simulator`], so every tool call crosses the actual
//! wire protocol (Hello/HelloAck, requests, streamed events). The tests assert
//! tool discovery, the `sim:ok hello` output path, honest failure and
//! unknown-on-disconnect outcomes (REQ-N-008), and that every tool result
//! embeds the session id and the simulated [`TargetIdentity`] (REQ-F-016).
//!
//! `unwrap`/`expect`/`panic` are allowed here: this is test-only code, and the
//! clippy in-test exemptions do not reach top-level helper functions in an
//! integration-test crate.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cretlink_console::{SessionClient, SessionTarget};
use cretlink_mcp::McpBridge;
use cretlink_protocol::SessionId;
use cretlink_simulator::Simulator;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, serve_client, serve_server};
use serde_json::{Map, Value, json};

/// Wires simulator <-> `SessionClient`/`SessionTarget` <-> `McpBridge` <-> a
/// real rmcp client, returning the client plus the wire session id the
/// simulator assigned.
async fn connect() -> (RunningService<RoleClient, ()>, SessionId) {
    let (client, session, _flag) = connect_with_flag().await;
    (client, session)
}

/// Like [`connect`], but also returns the simulator's mutable pending-reboot
/// flag so a test can flip it between calls (ST-M003-003).
async fn connect_with_flag() -> (RunningService<RoleClient, ()>, SessionId, Arc<AtomicBool>) {
    let (sim_io, console_io) = tokio::io::duplex(256 * 1024);
    let simulator = Simulator::new();
    let reboot_flag = simulator.reboot_flag();
    tokio::spawn(async move {
        let _ = simulator.serve(sim_io).await;
    });

    let session_client = SessionClient::connect(console_io, vec!["exec".to_string()])
        .await
        .expect("session handshake against the simulator");
    let session = session_client.session();
    let bridge = McpBridge::new(Arc::new(SessionTarget::new(Arc::new(session_client))));

    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        if let Ok(running) = serve_server(bridge, server_io).await {
            let _ = running.waiting().await;
        }
    });

    let mcp_client = serve_client((), client_io)
        .await
        .expect("rmcp client initialization");
    (mcp_client, session, reboot_flag)
}

fn structured(result: &rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("tool result carries structured content")
}

/// Asserts the REQ-F-016 envelope: the session id matches the wire session
/// and the target is unmistakably the simulator.
fn assert_identity(body: &Value, session: SessionId) {
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["target"]["simulated"], json!(true));
    assert_eq!(body["target"]["hostname"], json!("SIMULATED-TARGET"));
    assert_eq!(body["target"]["elevated"], json!(false));
}

fn exec_args(command: &str) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("shell".to_string(), json!("power_shell"));
    args.insert("command".to_string(), json!(command));
    args
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_all_five_generic_tools() {
    let (client, _session) = connect().await;

    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    for expected in [
        "remote_system_info",
        "remote_exec",
        "remote_write_stdin",
        "remote_cancel",
        "remote_list_jobs",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing tool {expected}"
        );
    }

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_info_reports_the_simulated_target_end_to_end() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(CallToolRequestParams::new("remote_system_info"))
        .await
        .expect("remote_system_info");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_identity(&body, session);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["result"]["target"]["simulated"], json!(true));
    assert_eq!(
        body["result"]["target"]["hostname"],
        json!("SIMULATED-TARGET")
    );
    // The report carries the agent-side values from the live SystemInfo
    // answer over the wire — not a console-side cache (ST-M003-003).
    assert_eq!(
        body["result"]["target"]["os_version"],
        json!("Simulated Windows")
    );
    assert_eq!(body["result"]["target"]["arch"], json!("x86_64"));
    assert_eq!(body["result"]["reboot_pending"], json!(false));

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_sim_ok_streams_output_end_to_end() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("remote_exec").with_arguments(exec_args("sim:ok hello")),
        )
        .await
        .expect("remote_exec");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_identity(&body, session);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["result"]["outcome"], json!("completed"));
    assert_eq!(body["result"]["exit_code"], json!(0));
    assert_eq!(body["result"]["truncated"], json!(false));
    // The effective remote working directory is surfaced end to end
    // (ST-M003-003, REQ-F-009); the simulator reports its VFS root.
    assert!(
        body["result"]["working_dir"]
            .as_str()
            .is_some_and(|dir| !dir.is_empty()),
        "the effective working directory must be reported, got {body}"
    );

    let encoded = body["result"]["stdout_base64"]
        .as_str()
        .expect("stdout_base64 string");
    let stdout = BASE64.decode(encoded).expect("valid base64");
    assert_eq!(stdout, b"hello\n");

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_sim_fail_reports_the_failed_outcome() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("remote_exec").with_arguments(exec_args("sim:fail 5")),
        )
        .await
        .expect("remote_exec");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_identity(&body, session);
    assert_eq!(body["result"]["outcome"], json!("failed"));
    assert_eq!(body["result"]["exit_code"], json!(5));

    let encoded = body["result"]["stderr_base64"]
        .as_str()
        .expect("stderr_base64 string");
    let stderr = BASE64.decode(encoded).expect("valid base64");
    assert!(!stderr.is_empty(), "sim:fail must produce stderr output");

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_sim_disconnect_reports_unknown_outcome() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("remote_exec").with_arguments(exec_args("sim:disconnect")),
        )
        .await
        .expect("remote_exec");
    assert_ne!(result.is_error, Some(true));

    // The simulator dropped the connection mid-job: the bridge must report the
    // honest unknown outcome, never a fabricated success (REQ-N-008).
    let body = structured(&result);
    assert_identity(&body, session);
    assert_eq!(body["result"]["outcome"], json!("unknown"));
    assert_eq!(body["result"]["exit_code"], json!(null));

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_jobs_reflects_the_finished_job() {
    let (client, session) = connect().await;

    let exec = client
        .call_tool(
            CallToolRequestParams::new("remote_exec").with_arguments(exec_args("sim:ok done")),
        )
        .await
        .expect("remote_exec");
    let job = structured(&exec)["result"]["job"].as_u64().expect("job id");

    let result = client
        .call_tool(CallToolRequestParams::new("remote_list_jobs"))
        .await
        .expect("remote_list_jobs");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_identity(&body, session);
    let jobs = body["result"]["jobs"].as_array().expect("jobs array");
    let listed = jobs
        .iter()
        .find(|entry| entry["job"] == json!(job))
        .expect("the executed job is listed");
    assert_eq!(listed["running"], json!(false));
    assert_eq!(listed["command_preview"], json!("sim:ok done"));

    client.cancel().await.ok();
}

/// Two `remote_system_info` calls observe a flip of the simulator's live
/// pending-reboot flag: proof the report is a per-request roundtrip to the
/// (simulated) agent, and that no console/bridge layer re-introduced a cached
/// fabrication (ST-M003-003).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_info_reflects_a_live_reboot_flag_flip() {
    let (client, session, reboot_flag) = connect_with_flag().await;

    let first = client
        .call_tool(CallToolRequestParams::new("remote_system_info"))
        .await
        .expect("first remote_system_info");
    let body = structured(&first);
    assert_identity(&body, session);
    assert_eq!(body["result"]["reboot_pending"], json!(false));

    reboot_flag.store(true, Ordering::SeqCst);

    let second = client
        .call_tool(CallToolRequestParams::new("remote_system_info"))
        .await
        .expect("second remote_system_info");
    assert_eq!(
        structured(&second)["result"]["reboot_pending"],
        json!(true),
        "the flip must be visible: the report is live, never a cache"
    );

    client.cancel().await.ok();
}

/// A remote `Unsupported` rejection travels end to end as a structured
/// `error_code` token beside `error_kind` (ST-M003-003, REQ-N-015).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_command_carries_the_structured_error_code() {
    let (client, session) = connect().await;

    // The simulator only runs sim:* scripted commands; anything else is an
    // honest remote Unsupported error.
    let result = client
        .call_tool(CallToolRequestParams::new("remote_exec").with_arguments(exec_args("format c:")))
        .await
        .expect("remote_exec returns a result envelope");
    assert_eq!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error_kind"], json!("remote"));
    assert_eq!(body["error_code"], json!("unsupported"));

    client.cancel().await.ok();
}

/// A desktop primitive routes end to end through the real wire, and a target
/// without a desktop backend (the simulator advertises no `desktop.v1`
/// capability) refuses it honestly: the bridge surfaces a structured
/// `unsupported` error rather than fabricating a frame (ADR-0015, REQ-N-008,
/// ST-M005-004).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_capture_is_refused_unsupported_end_to_end() {
    let (client, session) = connect().await;

    let mut args = Map::new();
    args.insert("display".to_string(), json!(0));
    args.insert("format".to_string(), json!("png"));

    let result = client
        .call_tool(CallToolRequestParams::new("remote_capture_desktop").with_arguments(args))
        .await
        .expect("remote_capture_desktop returns a result envelope");
    assert_eq!(
        result.is_error,
        Some(true),
        "a target without a desktop backend must refuse capture, not succeed"
    );

    let body = structured(&result);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error_kind"], json!("remote"));
    assert_eq!(
        body["error_code"],
        json!("unsupported"),
        "the honest refusal must carry the structured unsupported code, never a stale frame"
    );
    // No image content block may accompany an honest refusal.
    let has_image = result
        .content
        .iter()
        .any(|block| block.as_image().is_some());
    assert!(
        !has_image,
        "a refused capture must not carry a fabricated image"
    );

    client.cancel().await.ok();
}
