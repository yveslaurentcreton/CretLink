//! In-process rmcp end-to-end test for [`cretlink_mcp::McpBridge`].
//!
//! A real rmcp client (from the SDK) is connected to the bridge over a tokio
//! duplex "stdio", backed by a deterministic [`TargetSession`] fake. This
//! exercises the full MCP surface — tool discovery and calls — without a live
//! agent, and asserts every result carries the session id and a simulated
//! [`TargetIdentity`] (REQ-F-016). The simulator-backed variant of this test
//! lives in `bridge_simulator.rs`.
//!
//! `unwrap`/`expect`/`panic` are allowed here: this is test-only code, and the
//! clippy in-test exemptions do not reach top-level helper functions in an
//! integration-test crate.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cretlink_mcp::{
    CallError, CaptureParams, CaptureReport, CleanupSummary, DirPage, DownloadParams,
    DownloadReport, ExecParams, ExecReport, McpBridge, Routed, StatReport, SystemInfoReport,
    TargetError, TargetSession, UploadParams, UploadReport,
};
use cretlink_protocol::{
    B64Bytes, CleanupFailure, CleanupPolicy, CleanupTrigger, DesktopFrameMeta, DesktopState,
    DigestAlgo, ErrorCode, ErrorStage, FileEntry, FileKind, JobId, JobOutcome, JobSummary,
    KeyAction, PointerAction, PointerButton, Purpose, RemotePath, SessionId, StagedEntry,
    TargetIdentity,
};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, serve_client, serve_server};
use serde_json::{Map, Value, json};

/// A deterministic in-memory target. `remote_exec` echoes the command back as
/// stdout with a `completed` outcome, mirroring the simulator's `sim:ok`.
struct MockTarget {
    session: SessionId,
    target: TargetIdentity,
    /// Every stated purpose the bridge forwarded, in call order (ST-M009-007).
    purposes: Arc<Mutex<Vec<String>>>,
}

impl MockTarget {
    fn new() -> Self {
        Self {
            session: SessionId::new(),
            target: simulated_identity(),
            purposes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Records the purpose a call reached the target with.
    fn saw(&self, purpose: &Purpose) {
        self.purposes
            .lock()
            .unwrap()
            .push(purpose.as_str().to_string());
    }

    /// Labels a value with this target's fixed identity, as a real backend
    /// labels a result from the reply that produced it.
    fn routed<T>(&self, value: T) -> Routed<T> {
        Routed {
            session: self.session,
            target: self.target.clone(),
            // A fixed, non-zero mock call id: real backends carry the
            // console-minted audit `call_id` here (ST-M004-006).
            call_id: 42,
            value,
        }
    }
}

fn simulated_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "SIMULATED-TARGET".to_string(),
        os_version: "Simulated Windows".to_string(),
        arch: "x86_64".to_string(),
        elevated: false,
        simulated: true,
        agent_version: "mock-1".to_string(),
    }
}

#[async_trait]
impl TargetSession for MockTarget {
    fn session_id(&self) -> SessionId {
        self.session
    }

    fn target(&self) -> TargetIdentity {
        self.target.clone()
    }

    async fn system_info(&self, purpose: Purpose) -> Result<Routed<SystemInfoReport>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(SystemInfoReport {
            target: self.target.clone(),
            reboot_pending: false,
        }))
    }

    async fn exec(
        &self,
        purpose: Purpose,
        params: ExecParams,
        _cap: usize,
    ) -> Result<Routed<ExecReport>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(ExecReport {
            job: JobId(7),
            // Echo the requested directory (or a fixed default) so a test can
            // prove the effective working directory is surfaced (REQ-F-009).
            working_dir: params
                .working_dir
                .unwrap_or_else(|| "C:\\mock-cwd".to_string()),
            stdout: format!("ran: {}\n", params.command).into_bytes(),
            stderr: Vec::new(),
            truncated: false,
            exit_code: Some(0),
            duration_ms: 3,
            outcome: JobOutcome::Completed,
        }))
    }

    async fn write_stdin(
        &self,
        purpose: Purpose,
        _job: JobId,
        _data: Vec<u8>,
        _sensitive: bool,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn cancel(&self, purpose: Purpose, _job: JobId) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn list_jobs(&self, purpose: Purpose) -> Result<Routed<Vec<JobSummary>>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(vec![JobSummary {
            job: JobId(7),
            command_preview: "sim:ok hello".to_string(),
            running: false,
        }]))
    }

    async fn fs_stat(
        &self,
        purpose: Purpose,
        path: RemotePath,
    ) -> Result<Routed<StatReport>, CallError> {
        self.saw(&purpose);
        // Echo the addressed authority domain back into the entry name so a test
        // can prove the bridge forwarded the exact `RemotePath` variant.
        Ok(self.routed(StatReport {
            entry: Some(FileEntry {
                name: echo_path(&path),
                kind: FileKind::File,
                len: 4,
                modified_ms: Some(1_000),
                readonly: false,
            }),
            partial_len: None,
        }))
    }

    async fn fs_list(
        &self,
        purpose: Purpose,
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<Routed<DirPage>, CallError> {
        self.saw(&purpose);
        let name = match continue_after {
            Some(after) => format!("{}|after:{after}", echo_path(&path)),
            None => echo_path(&path),
        };
        Ok(self.routed(DirPage {
            entries: vec![FileEntry {
                name,
                kind: FileKind::Dir,
                len: 0,
                modified_ms: None,
                readonly: false,
            }],
            end: true,
        }))
    }

    async fn make_dir(&self, purpose: Purpose, _path: RemotePath) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn fs_move(
        &self,
        purpose: Purpose,
        _from: RemotePath,
        _to: RemotePath,
        _overwrite: bool,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn fs_delete(
        &self,
        purpose: Purpose,
        _path: RemotePath,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn remove_dir(
        &self,
        purpose: Purpose,
        _path: RemotePath,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn upload(
        &self,
        purpose: Purpose,
        params: UploadParams,
    ) -> Result<Routed<UploadReport>, CallError> {
        self.saw(&purpose);
        // The bridge always addresses staging by name; echo it into the
        // committed path so a test can prove the dest was forwarded.
        let name = match params.dest {
            RemotePath::Staging { name } => name,
            RemotePath::Absolute { path } => path,
        };
        Ok(self.routed(UploadReport {
            outcome: JobOutcome::Completed,
            remote_path: Some(format!("C:\\staging\\{name}")),
            len: 4,
            sent: 4,
            digest: vec![0xde, 0xad, 0xbe, 0xef],
            resumed_from: 0,
            restarted: false,
            error: None,
        }))
    }

    async fn download(
        &self,
        purpose: Purpose,
        params: DownloadParams,
    ) -> Result<Routed<DownloadReport>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(DownloadReport {
            outcome: JobOutcome::Completed,
            local_path: params.local_path,
            len: 4,
            sent: 4,
            digest: vec![0xde, 0xad, 0xbe, 0xef],
            resumed_from: 0,
            restarted: false,
            error: None,
        }))
    }

    async fn list_staged(&self, purpose: Purpose) -> Result<Routed<Vec<StagedEntry>>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(vec![StagedEntry {
            canonical_path: "C:\\staging\\report.txt".to_string(),
            source: "tech:report".to_string(),
            len: 4,
            algo: DigestAlgo::Sha256,
            digest: B64Bytes::new(vec![0xde, 0xad, 0xbe, 0xef]),
            cleanup: CleanupPolicy::OnSessionEnd,
            created_ms: 1_000,
        }]))
    }

    async fn cleanup(
        &self,
        purpose: Purpose,
        trigger: CleanupTrigger,
    ) -> Result<Routed<CleanupSummary>, CallError> {
        self.saw(&purpose);
        // Echo the trigger through the removed count so a test can prove it was
        // forwarded, and report one honest failure kept for retry (REQ-N-008).
        let removed = match trigger {
            CleanupTrigger::SessionEnd => 1,
            CleanupTrigger::Shutdown => 2,
            CleanupTrigger::All => 3,
        };
        Ok(self.routed(CleanupSummary {
            removed,
            failed: vec![CleanupFailure {
                canonical_path: "C:\\staging\\locked.bin".to_string(),
                reason: "in use".to_string(),
            }],
        }))
    }

    // --- Visual desktop control (ADR-0015, ST-M005-004) ---

    async fn capture_desktop(
        &self,
        purpose: Purpose,
        params: CaptureParams,
    ) -> Result<Routed<CaptureReport>, CallError> {
        self.saw(&purpose);
        // A deterministic, honest "live" frame: `Active` state with a tiny
        // inline PNG signature, echoing the requested display and format so a
        // test can prove the parameters were forwarded (REQ-F-016).
        Ok(self.routed(CaptureReport {
            meta: DesktopFrameMeta {
                state: DesktopState::Active,
                wts_session: 1,
                desktop: "WinSta0\\Default".to_string(),
                display: params.display,
                width: 8,
                height: 8,
                physical_width: 8,
                physical_height: 8,
                dpi: 96,
                format: params.format,
                captured_unix_ms: 1_000,
            },
            image: Some(B64Bytes::new(vec![0x89, 0x50, 0x4e, 0x47])),
            staged: None,
        }))
    }

    async fn desktop_pointer(
        &self,
        purpose: Purpose,
        _action: PointerAction,
        _button: PointerButton,
        _x: i32,
        _y: i32,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn desktop_scroll(
        &self,
        purpose: Purpose,
        _x: i32,
        _y: i32,
        _dx: i32,
        _dy: i32,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn desktop_key(
        &self,
        purpose: Purpose,
        _action: KeyAction,
        _key: String,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn desktop_type(&self, purpose: Purpose, _text: String) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn desktop_hotkey(
        &self,
        purpose: Purpose,
        _keys: Vec<String>,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn desktop_wait(&self, purpose: Purpose, _millis: u64) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }

    async fn clipboard_get(&self, purpose: Purpose) -> Result<Routed<Option<String>>, CallError> {
        self.saw(&purpose);
        // A deterministic, non-secret clipboard payload for round-trip tests.
        Ok(self.routed(Some("mock clipboard".to_string())))
    }

    async fn clipboard_set(
        &self,
        purpose: Purpose,
        _text: String,
    ) -> Result<Routed<()>, CallError> {
        self.saw(&purpose);
        Ok(self.routed(()))
    }
}

/// Renders a [`RemotePath`] as a domain-tagged string so a test assertion can
/// prove which disjoint authority domain the bridge forwarded (ADR-0012).
fn echo_path(path: &RemotePath) -> String {
    match path {
        RemotePath::Absolute { path } => format!("abs:{path}"),
        RemotePath::Staging { name } => format!("stg:{name}"),
    }
}

/// Spawns the bridge over one duplex half and returns a connected rmcp client
/// plus the session id its backing target reports.
async fn connect() -> (RunningService<RoleClient, ()>, SessionId) {
    let (client, session, _purposes) = connect_recording().await;
    (client, session)
}

/// Like [`connect`], also returning the purposes the mock target received.
async fn connect_recording() -> (
    RunningService<RoleClient, ()>,
    SessionId,
    Arc<Mutex<Vec<String>>>,
) {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let mock = MockTarget::new();
    let session = mock.session_id();
    let purposes = Arc::clone(&mock.purposes);
    let bridge = McpBridge::new(Arc::new(mock));

    tokio::spawn(async move {
        if let Ok(running) = serve_server(bridge, server_io).await {
            let _ = running.waiting().await;
        }
    });

    let client = serve_client((), client_io)
        .await
        .expect("client initialization");
    (client, session, purposes)
}

fn structured(result: &rmcp::model::CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("tool result carries structured content")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_all_five_generic_tools() {
    let (client, _session) = connect().await;

    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();

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
async fn tool_schemas_advertise_the_protocol_limits() {
    let (client, _session) = connect().await;
    let tools = client.list_all_tools().await.expect("list tools");

    // The advertised schemas must carry the same bounds the bridge enforces
    // at runtime, so schema and code cannot drift.
    let exec = tools
        .iter()
        .find(|tool| tool.name == "remote_exec")
        .expect("remote_exec listed");
    let schema = serde_json::to_value(exec.input_schema.as_ref()).expect("schema is json");
    assert_eq!(
        schema["properties"]["command"]["maxLength"],
        json!(cretlink_protocol::limits::MAX_COMMAND_BYTES),
        "command bound missing from the schema: {schema}"
    );
    assert_eq!(
        schema["properties"]["working_dir"]["maxLength"],
        json!(cretlink_protocol::limits::MAX_PATH_BYTES),
        "working_dir bound missing from the schema: {schema}"
    );
    assert_eq!(
        schema["properties"]["timeout_ms"]["maximum"],
        json!(cretlink_protocol::limits::MAX_TIMEOUT_MS),
        "timeout_ms bound missing from the schema: {schema}"
    );

    let stdin = tools
        .iter()
        .find(|tool| tool.name == "remote_write_stdin")
        .expect("remote_write_stdin listed");
    let schema = serde_json::to_value(stdin.input_schema.as_ref()).expect("schema is json");
    assert_eq!(
        schema["properties"]["data_base64"]["maxLength"],
        json!(cretlink_mcp::MAX_STDIN_BASE64_CHARS),
        "data_base64 bound missing from the schema: {schema}"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_info_embeds_session_and_simulated_target() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(with_purpose(CallToolRequestParams::new(
            "remote_system_info",
        )))
        .await
        .expect("remote_system_info");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["target"]["simulated"], json!(true));
    assert_eq!(body["target"]["hostname"], json!("SIMULATED-TARGET"));
    assert_eq!(body["result"]["reboot_pending"], json!(false));

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_runs_end_to_end_and_reports_output() {
    let (client, session) = connect().await;

    let mut args = Map::new();
    args.insert("shell".to_string(), json!("power_shell"));
    args.insert("command".to_string(), json!("sim:ok hello"));

    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_exec").with_arguments(args),
        ))
        .await
        .expect("remote_exec");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["target"]["simulated"], json!(true));
    assert_eq!(body["result"]["outcome"], json!("completed"));
    assert_eq!(body["result"]["exit_code"], json!(0));
    assert_eq!(
        body["result"]["working_dir"],
        json!("C:\\mock-cwd"),
        "the effective working directory must be surfaced (REQ-F-009)"
    );

    let encoded = body["result"]["stdout_base64"]
        .as_str()
        .expect("stdout_base64 string");
    let stdout = BASE64.decode(encoded).expect("valid base64");
    assert_eq!(String::from_utf8_lossy(&stdout), "ran: sim:ok hello\n");

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_command_is_a_tool_error_not_a_panic() {
    let (client, _session) = connect().await;

    let mut args = Map::new();
    args.insert("shell".to_string(), json!("cmd"));
    args.insert(
        "command".to_string(),
        json!("x".repeat(cretlink_protocol::limits::MAX_COMMAND_BYTES + 1)),
    );

    let outcome = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_exec").with_arguments(args),
        ))
        .await;
    assert!(
        outcome.is_err(),
        "oversized command must be rejected as a protocol error"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_base64_stdin_is_rejected() {
    let (client, _session) = connect().await;

    let mut args = Map::new();
    args.insert("job".to_string(), json!(7));
    args.insert("data_base64".to_string(), json!("not valid base64 !!!"));

    let outcome = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_write_stdin").with_arguments(args),
        ))
        .await;
    assert!(
        outcome.is_err(),
        "invalid base64 must be rejected as a protocol error"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_stdin_is_rejected_before_decoding() {
    let (client, _session) = connect().await;

    // One padded quad over the encoded-length bound: rejected on the
    // encoded string alone, before any decoded buffer is allocated.
    let mut args = Map::new();
    args.insert("job".to_string(), json!(7));
    args.insert(
        "data_base64".to_string(),
        json!("A".repeat(cretlink_mcp::MAX_STDIN_BASE64_CHARS + 4)),
    );

    let outcome = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_write_stdin").with_arguments(args),
        ))
        .await;
    assert!(
        outcome.is_err(),
        "stdin data over the chunk limit must be rejected"
    );

    client.cancel().await.ok();
}

// --- File and transfer tools (ST-M004-003) ---

/// Builds a `remote_exec`-style argument map from a single `(key, value)`.
fn args_of(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut args = Map::new();
    for (key, value) in pairs {
        args.insert((*key).to_string(), value.clone());
    }
    args
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_all_file_and_transfer_tools() {
    let (client, _session) = connect().await;

    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();

    for expected in [
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
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing tool {expected}"
        );
    }

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_tool_schemas_advertise_the_protocol_limits() {
    let (client, _session) = connect().await;
    let tools = client.list_all_tools().await.expect("list tools");

    // The upload schema's top-level string bounds must match the runtime checks
    // so the advertised surface and the enforced limits cannot drift.
    let upload = tools
        .iter()
        .find(|tool| tool.name == "remote_upload")
        .expect("remote_upload listed");
    let schema = serde_json::to_value(upload.input_schema.as_ref()).expect("schema is json");
    assert_eq!(
        schema["properties"]["local_path"]["maxLength"],
        json!(cretlink_protocol::limits::MAX_PATH_BYTES),
        "local_path bound missing from the schema: {schema}"
    );
    assert_eq!(
        schema["properties"]["dest_name"]["maxLength"],
        json!(cretlink_protocol::limits::MAX_NAME_BYTES),
        "dest_name bound missing from the schema: {schema}"
    );
    assert_eq!(
        schema["properties"]["source_label"]["maxLength"],
        json!(cretlink_protocol::limits::MAX_LABEL_BYTES),
        "source_label bound missing from the schema: {schema}"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stat_distinguishes_absolute_from_staging() {
    let (client, session) = connect().await;

    // An absolute path and a staging component are disjoint authority domains
    // (ADR-0012); the bridge must forward each variant unchanged.
    let absolute = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_stat").with_arguments(args_of(&[(
                "path",
                json!({ "kind": "absolute", "path": "C:\\Windows" }),
            )])),
        ))
        .await
        .expect("remote_stat absolute");
    assert_ne!(absolute.is_error, Some(true));
    let body = structured(&absolute);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["target"]["simulated"], json!(true));
    assert_eq!(body["result"]["entry"]["name"], json!("abs:C:\\Windows"));

    let staging = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_stat").with_arguments(args_of(&[(
                "path",
                json!({ "kind": "staging", "name": "report.txt" }),
            )])),
        ))
        .await
        .expect("remote_stat staging");
    assert_ne!(staging.is_error, Some(true));
    let body = structured(&staging);
    assert_eq!(body["result"]["entry"]["name"], json!("stg:report.txt"));

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_dir_forwards_pagination_token() {
    let (client, _session) = connect().await;

    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_list_dir").with_arguments(args_of(&[
                ("path", json!({ "kind": "absolute", "path": "C:\\logs" })),
                ("continue_after", json!("part-0009")),
            ])),
        ))
        .await
        .expect("remote_list_dir");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["result"]["end"], json!(true));
    assert_eq!(
        body["result"]["entries"][0]["name"],
        json!("abs:C:\\logs|after:part-0009"),
        "the directory path and the continue_after token must both be forwarded"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn make_dir_move_delete_remove_dir_acknowledge() {
    let (client, session) = connect().await;

    let staging = json!({ "kind": "staging", "name": "work" });
    let other = json!({ "kind": "staging", "name": "work2" });

    for (tool, args, key) in [
        (
            "remote_make_dir",
            args_of(&[("path", staging.clone())]),
            "created",
        ),
        (
            "remote_move",
            args_of(&[
                ("from", staging.clone()),
                ("to", other),
                ("overwrite", json!(true)),
            ]),
            "moved",
        ),
        (
            "remote_delete",
            args_of(&[("path", staging.clone())]),
            "deleted",
        ),
        (
            "remote_remove_dir",
            args_of(&[("path", staging.clone())]),
            "removed",
        ),
    ] {
        let result = client
            .call_tool(with_purpose(
                CallToolRequestParams::new(tool).with_arguments(args),
            ))
            .await
            .unwrap_or_else(|err| panic!("{tool} failed: {err}"));
        assert_ne!(result.is_error, Some(true), "{tool} returned an error");
        let body = structured(&result);
        assert_eq!(body["session_id"], json!(session.to_string()), "{tool}");
        assert_eq!(body["target"]["simulated"], json!(true), "{tool}");
        assert_eq!(body["result"][key], json!(true), "{tool} ack missing");
    }

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_reports_hex_digest_and_forwards_dest() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_upload").with_arguments(args_of(&[
                ("local_path", json!("C:\\tech\\report.txt")),
                ("dest_name", json!("report.txt")),
                ("source_label", json!("tech:report")),
                ("cleanup", json!("on_session_end")),
            ])),
        ))
        .await
        .expect("remote_upload");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["result"]["outcome"], json!("completed"));
    assert_eq!(
        body["result"]["digest_hex"],
        json!("deadbeef"),
        "the transfer digest must be surfaced as lowercase hex"
    );
    assert_eq!(
        body["result"]["remote_path"],
        json!("C:\\staging\\report.txt"),
        "the staging dest name must be forwarded"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_reports_hex_digest_and_local_path() {
    let (client, _session) = connect().await;

    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_download").with_arguments(args_of(&[
                (
                    "source",
                    json!({ "kind": "absolute", "path": "C:\\logs\\a.txt" }),
                ),
                ("local_path", json!("C:\\tech\\a.txt")),
            ])),
        ))
        .await
        .expect("remote_download");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["result"]["outcome"], json!("completed"));
    assert_eq!(body["result"]["digest_hex"], json!("deadbeef"));
    assert_eq!(body["result"]["local_path"], json!("C:\\tech\\a.txt"));

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_staged_reports_hex_digest() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(with_purpose(CallToolRequestParams::new(
            "remote_list_staged",
        )))
        .await
        .expect("remote_list_staged");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["session_id"], json!(session.to_string()));
    let entry = &body["result"]["staged"][0];
    assert_eq!(entry["canonical_path"], json!("C:\\staging\\report.txt"));
    assert_eq!(entry["source"], json!("tech:report"));
    assert_eq!(
        entry["digest_hex"],
        json!("deadbeef"),
        "the staged digest must be surfaced as lowercase hex, not base64"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_forwards_trigger_and_reports_failures() {
    let (client, _session) = connect().await;

    // The mock echoes the trigger through `removed` (all => 3) so a mismatch
    // would prove the wrong trigger was forwarded.
    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_cleanup")
                .with_arguments(args_of(&[("trigger", json!("all"))])),
        ))
        .await
        .expect("remote_cleanup");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["result"]["removed"], json!(3));
    assert_eq!(
        body["result"]["failed"][0]["reason"],
        json!("in use"),
        "per-artifact cleanup failures must be reported honestly"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_upload_source_label_is_rejected() {
    let (client, _session) = connect().await;

    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_upload").with_arguments(args_of(&[
                ("local_path", json!("C:\\tech\\report.txt")),
                ("dest_name", json!("report.txt")),
                (
                    "source_label",
                    json!("x".repeat(cretlink_protocol::limits::MAX_LABEL_BYTES + 1)),
                ),
                ("cleanup", json!("on_session_end")),
            ])),
        ))
        .await;
    assert!(
        result.is_err(),
        "an over-limit source_label must be rejected as a protocol error"
    );

    client.cancel().await.ok();
}

// --- Structured error classes in the envelope (ST-M003-003, REQ-N-015) ---

/// A target whose every call fails with one configured [`TargetError`], so the
/// envelope's error fields can be asserted per failure class.
struct FailingTarget {
    session: SessionId,
    target: TargetIdentity,
    error: TargetError,
}

impl FailingTarget {
    fn new(error: TargetError) -> Self {
        Self {
            session: SessionId::new(),
            target: simulated_identity(),
            error,
        }
    }

    fn fail<T>(&self) -> Result<Routed<T>, CallError> {
        // Attach a fixed call_id so the error envelope's correlation field can
        // be asserted (ST-M004-006).
        Err(CallError::new(self.error.clone(), Some(FAILING_CALL_ID)))
    }
}

/// The `call_id` [`FailingTarget`] reports on every failure.
const FAILING_CALL_ID: u64 = 77;

#[async_trait]
impl TargetSession for FailingTarget {
    fn session_id(&self) -> SessionId {
        self.session
    }

    fn target(&self) -> TargetIdentity {
        self.target.clone()
    }

    async fn system_info(&self, _purpose: Purpose) -> Result<Routed<SystemInfoReport>, CallError> {
        self.fail()
    }

    async fn exec(
        &self,
        _purpose: Purpose,
        _params: ExecParams,
        _cap: usize,
    ) -> Result<Routed<ExecReport>, CallError> {
        self.fail()
    }

    async fn write_stdin(
        &self,
        _purpose: Purpose,
        _job: JobId,
        _data: Vec<u8>,
        _sensitive: bool,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn cancel(&self, _purpose: Purpose, _job: JobId) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn list_jobs(&self, _purpose: Purpose) -> Result<Routed<Vec<JobSummary>>, CallError> {
        self.fail()
    }

    async fn fs_stat(
        &self,
        _purpose: Purpose,
        _path: RemotePath,
    ) -> Result<Routed<StatReport>, CallError> {
        self.fail()
    }

    async fn fs_list(
        &self,
        _purpose: Purpose,
        _path: RemotePath,
        _continue_after: Option<String>,
    ) -> Result<Routed<DirPage>, CallError> {
        self.fail()
    }

    async fn make_dir(
        &self,
        _purpose: Purpose,
        _path: RemotePath,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn fs_move(
        &self,
        _purpose: Purpose,
        _from: RemotePath,
        _to: RemotePath,
        _overwrite: bool,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn fs_delete(
        &self,
        _purpose: Purpose,
        _path: RemotePath,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn remove_dir(
        &self,
        _purpose: Purpose,
        _path: RemotePath,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn upload(
        &self,
        _purpose: Purpose,
        _params: UploadParams,
    ) -> Result<Routed<UploadReport>, CallError> {
        self.fail()
    }

    async fn download(
        &self,
        _purpose: Purpose,
        _params: DownloadParams,
    ) -> Result<Routed<DownloadReport>, CallError> {
        self.fail()
    }

    async fn list_staged(&self, _purpose: Purpose) -> Result<Routed<Vec<StagedEntry>>, CallError> {
        self.fail()
    }

    async fn cleanup(
        &self,
        _purpose: Purpose,
        _trigger: CleanupTrigger,
    ) -> Result<Routed<CleanupSummary>, CallError> {
        self.fail()
    }

    // --- Visual desktop control (ADR-0015, ST-M005-004) ---

    async fn capture_desktop(
        &self,
        _purpose: Purpose,
        _params: CaptureParams,
    ) -> Result<Routed<CaptureReport>, CallError> {
        self.fail()
    }

    async fn desktop_pointer(
        &self,
        _purpose: Purpose,
        _action: PointerAction,
        _button: PointerButton,
        _x: i32,
        _y: i32,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn desktop_scroll(
        &self,
        _purpose: Purpose,
        _x: i32,
        _y: i32,
        _dx: i32,
        _dy: i32,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn desktop_key(
        &self,
        _purpose: Purpose,
        _action: KeyAction,
        _key: String,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn desktop_type(
        &self,
        _purpose: Purpose,
        _text: String,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn desktop_hotkey(
        &self,
        _purpose: Purpose,
        _keys: Vec<String>,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn desktop_wait(&self, _purpose: Purpose, _millis: u64) -> Result<Routed<()>, CallError> {
        self.fail()
    }

    async fn clipboard_get(&self, _purpose: Purpose) -> Result<Routed<Option<String>>, CallError> {
        self.fail()
    }

    async fn clipboard_set(
        &self,
        _purpose: Purpose,
        _text: String,
    ) -> Result<Routed<()>, CallError> {
        self.fail()
    }
}

/// Connects an rmcp client to a bridge over a target that always fails with
/// `error`.
async fn connect_failing(error: TargetError) -> RunningService<RoleClient, ()> {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let bridge = McpBridge::new(Arc::new(FailingTarget::new(error)));
    tokio::spawn(async move {
        if let Ok(running) = serve_server(bridge, server_io).await {
            let _ = running.waiting().await;
        }
    });
    serve_client((), client_io)
        .await
        .expect("client initialization")
}

/// The four failure classes — missing path, denied access, unsupported
/// feature, disconnected target — are distinguishable through the structured
/// envelope fields: `error_kind` plus, for remote failures, the agent's
/// machine-readable `error_code` token (never only free text).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_envelope_distinguishes_the_four_failure_classes() {
    let remote = |code: ErrorCode, message: &str| TargetError::Remote {
        code,
        message: message.to_string(),
        stage: ErrorStage::Execution,
    };
    let cases: [(TargetError, &str, Option<&str>); 4] = [
        (
            remote(ErrorCode::PathNotFound, "no such path"),
            "remote",
            Some("path_not_found"),
        ),
        (
            remote(ErrorCode::AccessDenied, "access is denied"),
            "remote",
            Some("access_denied"),
        ),
        (
            remote(ErrorCode::Unsupported, "not supported here"),
            "remote",
            Some("unsupported"),
        ),
        (TargetError::Disconnected, "disconnected", None),
    ];

    for (error, kind, code) in cases {
        let client = connect_failing(error).await;
        let result = client
            .call_tool(with_purpose(
                CallToolRequestParams::new("remote_stat").with_arguments(args_of(&[(
                    "path",
                    json!({ "kind": "absolute", "path": "C:\\Windows" }),
                )])),
            ))
            .await
            .expect("the tool call returns a result envelope");
        assert_eq!(result.is_error, Some(true), "{kind} must be a tool error");

        let body = structured(&result);
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["error_kind"], json!(kind));
        // A failed routed call still carries its console-minted call_id, so the
        // error result correlates with its audit record (ST-M004-006).
        assert_eq!(
            body["call_id"],
            json!(FAILING_CALL_ID),
            "an error envelope must carry the failed call's call_id"
        );
        match code {
            Some(token) => assert_eq!(
                body["error_code"],
                json!(token),
                "a remote failure carries the structured error code"
            ),
            None => assert!(
                body.as_object()
                    .is_some_and(|map| !map.contains_key("error_code")),
                "a non-remote failure carries no error_code field, got {body}"
            ),
        }

        client.cancel().await.ok();
    }
}

/// An adversarial `working_dir` is rejected **bridge-locally** as invalid
/// parameters: no result envelope is produced and nothing crosses the IPC
/// toward a console or target (ST-M003-003, REQ-N-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adversarial_working_dir_is_rejected_at_the_bridge() {
    let (client, _session) = connect().await;

    for bad in ["C:\\Users\\..\\Windows", "\\\\server\\share", "C:"] {
        let result = client
            .call_tool(with_purpose(
                CallToolRequestParams::new("remote_exec").with_arguments(args_of(&[
                    ("shell", json!("cmd")),
                    ("command", json!("echo should-not-run")),
                    ("working_dir", json!(bad)),
                ])),
            ))
            .await;
        assert!(
            result.is_err(),
            "working_dir {bad:?} must fail as invalid params before any envelope, got {result:?}"
        );
    }

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_absolute_path_is_rejected() {
    let (client, _session) = connect().await;

    let result = client
        .call_tool(with_purpose(CallToolRequestParams::new("remote_stat").with_arguments(args_of(&[(
            "path",
            json!({
                "kind": "absolute",
                "path": "C:\\".to_string() + &"a".repeat(cretlink_protocol::limits::MAX_PATH_BYTES),
            }),
        )]))))
        .await;
    assert!(
        result.is_err(),
        "an over-limit absolute path must be rejected as a protocol error"
    );

    client.cancel().await.ok();
}

// --- Visual desktop control tools (ADR-0015, ST-M005-004) ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_all_desktop_tools() {
    let (client, _session) = connect().await;

    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();

    for expected in [
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
            "missing desktop tool {expected}"
        );
    }

    client.cancel().await.ok();
}

/// A captured `active` frame surfaces its honest desktop state, forwards the
/// requested display and format unchanged, and rides a live inline image both
/// as structured metadata and as a native MCP image content block (REQ-F-016).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_desktop_reports_active_frame_with_inline_image() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_capture_desktop")
                .with_arguments(args_of(&[("display", json!(3)), ("format", json!("webp"))])),
        ))
        .await
        .expect("remote_capture_desktop");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["target"]["simulated"], json!(true));
    assert_eq!(body["ok"], json!(true));
    assert_eq!(
        body["result"]["meta"]["state"],
        json!("active"),
        "the honest desktop state must be surfaced"
    );
    // The requested display and format are echoed back through the frame meta,
    // proving the capture parameters were forwarded unchanged.
    assert_eq!(body["result"]["meta"]["display"], json!(3));
    assert_eq!(body["result"]["meta"]["format"], json!("webp"));
    assert_eq!(body["result"]["image_inline"], json!(true));
    assert_eq!(body["result"]["image_bytes"], json!(4));
    assert_eq!(
        body["result"]["staged"],
        json!(null),
        "a small inline frame is not also staged"
    );

    // The live frame is additionally carried as a native MCP image content
    // block, not only as base64 buried in the JSON body.
    let has_image = result
        .content
        .iter()
        .any(|block| block.as_image().is_some());
    assert!(
        has_image,
        "an active frame must carry an inline MCP image content block"
    );

    client.cancel().await.ok();
}

/// Clipboard text round-trips through the read tool: the bridge surfaces the
/// target's clipboard payload verbatim under `result.text` (ST-M005-004).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clipboard_get_round_trips_text() {
    let (client, session) = connect().await;

    let result = client
        .call_tool(with_purpose(CallToolRequestParams::new(
            "remote_clipboard_get",
        )))
        .await
        .expect("remote_clipboard_get");
    assert_ne!(result.is_error, Some(true));

    let body = structured(&result);
    assert_eq!(body["session_id"], json!(session.to_string()));
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["result"]["text"], json!("mock clipboard"));

    client.cancel().await.ok();
}

/// Every non-capture desktop op (pointer, scroll, key, type, hotkey, wait, and
/// clipboard-set) is a routed acknowledgement: it returns the target identity
/// and the console-minted `call_id` without a payload of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desktop_input_ops_acknowledge_with_identity() {
    let (client, session) = connect().await;

    let calls: [(&str, Map<String, Value>); 7] = [
        (
            "remote_desktop_pointer",
            args_of(&[
                ("action", json!("click")),
                ("button", json!("left")),
                ("x", json!(100)),
                ("y", json!(200)),
            ]),
        ),
        (
            "remote_desktop_scroll",
            args_of(&[
                ("x", json!(10)),
                ("y", json!(20)),
                ("dx", json!(0)),
                ("dy", json!(-3)),
            ]),
        ),
        (
            "remote_desktop_key",
            args_of(&[("action", json!("press")), ("key", json!("Enter"))]),
        ),
        ("remote_desktop_type", args_of(&[("text", json!("hello"))])),
        (
            "remote_desktop_hotkey",
            args_of(&[("keys", json!(["ctrl", "shift", "esc"]))]),
        ),
        ("remote_desktop_wait", args_of(&[("millis", json!(50))])),
        (
            "remote_clipboard_set",
            args_of(&[("text", json!("copied text"))]),
        ),
    ];

    for (tool, args) in calls {
        let result = client
            .call_tool(with_purpose(
                CallToolRequestParams::new(tool).with_arguments(args),
            ))
            .await
            .unwrap_or_else(|err| panic!("{tool} must return a result: {err}"));
        assert_ne!(result.is_error, Some(true), "{tool} must acknowledge");

        let body = structured(&result);
        assert_eq!(body["ok"], json!(true), "{tool} must report ok");
        assert_eq!(
            body["session_id"],
            json!(session.to_string()),
            "{tool} must carry the session identity"
        );
        assert_eq!(
            body["target"]["simulated"],
            json!(true),
            "{tool} must carry the target identity"
        );
        assert_eq!(
            body["call_id"],
            json!(42),
            "{tool} must carry the console-minted call_id"
        );
    }

    client.cancel().await.ok();
}

/// A pointer coordinate whose magnitude exceeds the protocol bound is rejected
/// bridge-locally as invalid parameters: nothing crosses the IPC toward a
/// console or target (ST-M003-003, REQ-N-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_range_pointer_coordinate_is_rejected_at_the_bridge() {
    let (client, _session) = connect().await;

    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_desktop_pointer").with_arguments(args_of(&[
                ("action", json!("move")),
                ("button", json!("left")),
                (
                    "x",
                    json!(i64::from(cretlink_protocol::limits::MAX_DESKTOP_COORD) + 1),
                ),
                ("y", json!(0)),
            ])),
        ))
        .await;
    assert!(
        result.is_err(),
        "an out-of-range coordinate must be rejected as a protocol error"
    );

    client.cancel().await.ok();
}

/// Oversized type text is rejected bridge-locally before any wire request, on
/// the byte length alone (ST-M003-003, REQ-N-012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_desktop_type_is_rejected_at_the_bridge() {
    let (client, _session) = connect().await;

    let result = client
        .call_tool(with_purpose(
            CallToolRequestParams::new("remote_desktop_type").with_arguments(args_of(&[(
                "text",
                json!("x".repeat(cretlink_protocol::limits::MAX_DESKTOP_TEXT_BYTES + 1)),
            )])),
        ))
        .await;
    assert!(
        result.is_err(),
        "type text over the byte limit must be rejected as a protocol error"
    );

    client.cancel().await.ok();
}

/// The stated purpose every test tool call carries (ST-M009-007).
const TEST_PURPOSE: &str = "Checking the computer for the support request";

/// Adds the required stated `purpose` argument to a tool call that does not set
/// one (ST-M009-007), so each test states only the arguments it is about.
fn with_purpose(mut params: CallToolRequestParams) -> CallToolRequestParams {
    params
        .arguments
        .get_or_insert_with(Map::new)
        .entry("purpose")
        .or_insert_with(|| Value::String(TEST_PURPOSE.to_string()));
    params
}

// --- Stated purpose (REQ-F-026, ST-M009-007) -------------------------------

/// Every tool's schema requires `purpose`, and a valid one reaches the target
/// trimmed, exactly once per call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tool_requires_a_purpose_and_forwards_it_trimmed() {
    let (client, _session, purposes) = connect_recording().await;

    let tools = client.list_all_tools().await.expect("list tools");
    assert_eq!(tools.len(), 24);
    for tool in &tools {
        let schema = serde_json::to_value(tool.input_schema.as_ref()).unwrap();
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(
            required.contains(&json!("purpose")),
            "{} must require a purpose: {schema}",
            tool.name
        );
        assert_eq!(
            schema["properties"]["purpose"]["maxLength"],
            json!(cretlink_protocol::limits::MAX_PURPOSE_CHARS),
            "{}",
            tool.name
        );
    }

    let result = client
        .call_tool(
            CallToolRequestParams::new("remote_system_info").with_arguments(args_of(&[(
                "purpose",
                json!("  Checking which computer this is  "),
            )])),
        )
        .await
        .expect("system_info");
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        purposes.lock().unwrap().as_slice(),
        &["Checking which computer this is".to_string()]
    );

    client.cancel().await.ok();
}

/// A call without a valid purpose is refused with an actionable error that
/// names the argument, and nothing reaches the target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_without_a_valid_purpose_never_reaches_the_target() {
    let (client, _session, purposes) = connect_recording().await;
    let too_long = "a".repeat(cretlink_protocol::limits::MAX_PURPOSE_CHARS + 1);
    let cases: [(&str, Option<Value>); 6] = [
        ("missing", None),
        ("empty", Some(json!(""))),
        ("whitespace", Some(json!(" \t "))),
        (
            "multi-line",
            Some(json!("Checking disk\nand deleting files")),
        ),
        ("too long", Some(json!(too_long))),
        ("not a string", Some(json!(7))),
    ];
    for tool in ["remote_exec", "remote_system_info"] {
        for (case, purpose) in &cases {
            let mut args = if tool == "remote_exec" {
                args_of(&[("shell", json!("cmd")), ("command", json!("echo hi"))])
            } else {
                Map::new()
            };
            if let Some(purpose) = purpose {
                args.insert("purpose".to_string(), purpose.clone());
            }
            let message = match client
                .call_tool(CallToolRequestParams::new(tool).with_arguments(args))
                .await
            {
                Err(err) => err.to_string(),
                Ok(result) => {
                    assert_eq!(result.is_error, Some(true), "{tool}/{case}: {result:?}");
                    assert!(
                        result.structured_content.is_none(),
                        "{tool}/{case}: no routed envelope for a refused call"
                    );
                    format!("{:?}", result.content)
                }
            };
            // serde's type-mismatch message names the expected type, not the
            // field; every other rejection names the purpose argument.
            assert!(
                message.contains("purpose") || *case == "not a string",
                "{tool}/{case}: the error must name the purpose: {message}"
            );
        }
    }
    assert!(
        purposes.lock().unwrap().is_empty(),
        "no refused call may reach the target"
    );

    client.cancel().await.ok();
}
